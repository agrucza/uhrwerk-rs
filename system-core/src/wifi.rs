//! WiFi: session-based radio use - scan, and NTP time sync.
//!
//! The radio exists only for the seconds a session runs - the same
//! rail-gated model as the GPS sessions. Two session kinds, one per
//! [`WifiCommand`]:
//!
//! * `Scan`: bring the radio up, list the visible access points,
//!   tear down. Entries stream to the main loop one per event
//!   (strongest first, SSIDs deduplicated, hidden networks dropped).
//! * `SyncOnce`: bring the radio up, join the given AP, get a DHCP
//!   lease, do one SNTP exchange, hand the time to the shared RTC
//!   task, tear down.
//!
//! Dropping the `WifiController` deinitializes the WiFi driver and
//! stops the radio (esp-radio documents this on the controller's
//! `Drop`), so between sessions the radio contributes nothing to
//! sleep current and its heap is returned.
//!
//! Every session holds a [`bus::WakeHold`] for its duration: the
//! radio does not survive hardware light sleep, so the heartbeat
//! must not fire mid-session. Outside sessions the boards sleep
//! exactly as before.
//!
//! Board-agnostic like the audio session layer: esp-radio's API is
//! chip-neutral the same way esp-hal's is - the leaf bin's chip
//! feature selects the silicon. Gated behind this crate's `wifi`
//! cargo feature; `manager::run` spawns the task (taking the `WIFI`
//! peripheral through `Bringup::take_wifi`) and sets the wifi
//! capability from the same feature, so the UI row exists exactly
//! where the task does. Credentials arrive inside the command - this
//! crate stays config-blind, the model owns the stored network.
//!
//! Progress is published as `SystemEvent::WifiStatusUpdated`
//! (plus `WifiScanEntry` per network) for the settings WIFI views.

use app_core::data::{
    looks_like_file_server_token_guess, FileServerEvent, WifiFailure, WifiNetwork,
    WifiState, FILE_SERVER_TOKEN_ALPHABET, FILE_SERVER_TOKEN_LEN, MAX_WIFI_NETWORKS,
};
use app_core::events::SystemEvent;
use embassy_futures::select::{select, select3, Either, Either3};
// Two `Write`s, both used here and neither namable at a call site:
// `core::fmt` backs the `write!` macro into heapless strings, and
// `embedded_io_async` backs `write_all` on the socket.
use core::fmt::Write as _;
use embedded_io_async::Write as _;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, StackResources};
use embassy_time::{with_timeout, Duration};
use esp_hal::peripherals as p;
use esp_hal::rng::Rng;
use esp_hal::time::Duration as HalDuration;
use esp_radio::wifi::scan::{ScanConfig, ScanTypeConfig};
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethod, Config as WifiConfig, ControllerConfig, DisconnectReason,
    WifiController, WifiError,
};
use heapless::Vec;

use crate::bus::{self, RtcCommand, WifiCommand, EVENTS, RTC_COMMAND, WIFI_COMMAND};
use crate::clock_math::add_minutes;

use alloc::string::String;

/// Whole sync-session budget: radio init through RTC set. Covers AP
/// association, DHCP and walking the entire NTP fallback list with
/// slack; an absent AP or dead uplink ends the session here instead
/// of hanging it.
const SYNC_BUDGET_SECS: u64 = 45;

/// Scan-session budget. An active all-channel scan takes ~1-2 s.
const SCAN_BUDGET_SECS: u64 = 10;

/// How long to wait for the DHCP lease after association before
/// calling the network dead.
const LEASE_BUDGET_SECS: u64 = 15;

/// How long one NTP reply may take. Short on purpose: a live server
/// answers in well under a second on a LAN, and the whole fallback
/// walk (3 servers x [`NTP_ATTEMPTS`]) has to fit inside
/// [`SYNC_BUDGET_SECS`] alongside association and DHCP.
const NTP_REPLY_SECS: u64 = 3;

/// NTP servers, tried in order until one answers - resolved per
/// session via the DHCP-provided DNS server. Deliberately DIFFERENT
/// hostnames, not one name queried repeatedly: smoltcp's DNS returns
/// a single address per query (DNS_MAX_RESULT_COUNT = 1) and the
/// router's cache pins a name to the same address for its TTL - so
/// when the pool hands out a dead member, re-querying pool.ntp.org
/// keeps returning that same dead IP for minutes and every sync
/// times out (observed 2026-08-20). Each fallback name is its own
/// cache entry; Cloudflare and Google NTP are anycast.
/// The walk was verified on hardware 2026-08-20 with a TEST-NET-1
/// address wedged in at the front: both attempts timed out, the
/// session moved on to the next name and synced.
///
/// ANYCAST FIRST, pool last (reordered 2026-08-20): `pool.ntp.org`
/// hands out a random volunteer server, and on this network it drew a
/// dead one twice - each time costing both attempts (6 s of a sync)
/// before the walk reached a server that answered instantly. The
/// anycast services route to whatever instance is nearest and up, so
/// they are the right first try; the pool stays as the vendor-neutral
/// fallback for networks that block the big providers.
const NTP_SERVERS: &[&str] =
    &["time.cloudflare.com", "time.google.com", "pool.ntp.org"];

/// SNTP attempts per server before moving to the next: a lone UDP
/// datagram can simply be lost - one loss must not skip a live
/// server.
const NTP_ATTEMPTS: u32 = 2;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch
/// (1970-01-01).
const NTP_UNIX_OFFSET: u32 = 2_208_988_800;

/// Longest the task will wait for room in the event channel before
/// giving up on one status line (see [`publish`]).
const PUBLISH_WAIT_SECS: u64 = 2;

/// Owns the WIFI peripheral across sessions; each session borrows it
/// via `reborrow()` so the token is ready again for the next one -
/// the same session-scoped peripheral pattern as audio's I2S0.
#[embassy_executor::task]
pub async fn wifi_task(
    mut wifi: p::WIFI<'static>,
    store: &'static bus::SharedStore,
) {
    loop {
        let cmd = WIFI_COMMAND.wait().await;
        // Hardware light sleep would gate the radio's clocks
        // mid-session; hold the wake lock for the whole session
        // (released on every exit path by RAII).
        let _wake = bus::WakeHold::new();
        match cmd {
            WifiCommand::Scan => {
                log::info!("WiFi: scan session start");
                run_scan_session(wifi.reborrow()).await;
            }
            WifiCommand::SyncOnce { ssid, passphrase, tz_offset_minutes } => {
                log::info!("WiFi: sync session start ({})", ssid.as_str());
                run_sync_session(
                    wifi.reborrow(),
                    ssid.as_str(),
                    passphrase.as_str(),
                    tz_offset_minutes,
                )
                .await;
            }
            WifiCommand::Serve { ssid, passphrase } => {
                log::info!("WiFi: serve session start ({})", ssid.as_str());
                run_serve_session(
                    wifi.reborrow(),
                    ssid.as_str(),
                    passphrase.as_str(),
                    store,
                )
                .await;
            }
        }
        log::info!("WiFi: session done - radio off");
    }
}

/// Publish session progress for the settings WIFI views.
///
/// Prefers waiting for a free slot over dropping - a lost terminal
/// state would leave the UI stuck on SCANNING / CONNECTING - but the
/// wait is BOUNDED. An unbounded `send().await` here would park the
/// session task inside a full-channel wait, and a task parked
/// anywhere but `WIFI_COMMAND.wait()` silently swallows every later
/// command: the radio would then appear dead until reboot. A dropped
/// status line is a cosmetic bug; a wedged task is not.
async fn publish(state: WifiState) {
    let event = SystemEvent::WifiStatusUpdated { state };
    if EVENTS.try_send(event.clone()).is_ok() {
        return;
    }
    if with_timeout(Duration::from_secs(PUBLISH_WAIT_SECS), EVENTS.send(event))
        .await
        .is_err()
    {
        log::warn!("WiFi: event channel full - status {:?} dropped", state);
    }
}

// -- Scan session --------------------------------------------------------------

/// One scan session. The controller comes up in station mode by
/// default (esp-radio's `new` documents it), so no station config is
/// needed before scanning.
async fn run_scan_session(wifi: p::WIFI<'_>) {
    publish(WifiState::Scanning).await;
    let (mut controller, _interfaces) =
        match esp_radio::wifi::new(wifi, ControllerConfig::default()) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("WiFi: radio init failed: {:?}", e);
                publish(WifiState::Failed(WifiFailure::RadioInit)).await;
                return;
            }
        };

    // Ask for more than we keep: the dedup below collapses multi-AP
    // networks (mesh, repeaters) that each take a result slot.
    //
    // Dwell per channel: esp-radio's default active scan waits only
    // 10-20 ms, but APs beacon every ~100 ms - each pass then misses
    // a different subset and the list flickers between scans
    // (observed on the C6 2026-08-20: 6/8/9/9 networks with changing
    // members). One full beacon interval minimum per channel makes a
    // pass see what is actually there, at ~1.5-3 s for 13 channels.
    let config = ScanConfig::default()
        .with_max(2 * MAX_WIFI_NETWORKS)
        .with_scan_type(ScanTypeConfig::Active {
            min: HalDuration::from_millis(120),
            max: HalDuration::from_millis(240),
        });
    let scan = with_timeout(
        Duration::from_secs(SCAN_BUDGET_SECS),
        controller.scan_async(&config),
    )
    .await;
    let aps = match scan {
        Ok(Ok(aps)) => aps,
        Ok(Err(e)) => {
            log::warn!("WiFi: scan failed: {:?}", e);
            publish(WifiState::Failed(WifiFailure::ScanFailed)).await;
            return;
        }
        Err(_) => {
            log::warn!("WiFi: scan budget ({}s) exhausted", SCAN_BUDGET_SECS);
            publish(WifiState::Failed(WifiFailure::ScanFailed)).await;
            return;
        }
    };

    // Strongest first, one entry per SSID (the strongest BSSID of a
    // multi-AP network), hidden networks (empty SSID) dropped - the
    // list is for picking a name to type a passphrase for.
    let mut list: Vec<WifiNetwork, MAX_WIFI_NETWORKS> = Vec::new();
    let mut sorted = aps;
    sorted.sort_unstable_by(|a, b| b.signal_strength.cmp(&a.signal_strength));
    for ap in sorted.iter() {
        let name = ap.ssid.as_str();
        if name.is_empty() || list.iter().any(|n| n.ssid.as_str() == name) {
            continue;
        }
        let mut ssid = heapless::String::new();
        if ssid.push_str(name).is_err() {
            // Longer than the 802.11 maximum - not a real SSID.
            continue;
        }
        let secured = !matches!(ap.auth_method, Some(AuthenticationMethod::None));
        let net = WifiNetwork { ssid, rssi: ap.signal_strength, secured };
        if list.push(net).is_err() {
            break;
        }
    }
    log::info!("WiFi: scan found {} APs, {} networks listed", sorted.len(), list.len());
    for net in list.iter() {
        log::info!(
            "WiFi:   {:<32} {:>4} dBm {}",
            net.ssid.as_str(),
            net.rssi,
            if net.secured { "secured" } else { "open" },
        );
        // Bounded like `publish`: a dropped entry reappears on the
        // next refresh pass, a wedged task does not recover.
        let entry = SystemEvent::WifiScanEntry { network: net.clone() };
        if EVENTS.try_send(entry.clone()).is_err()
            && with_timeout(Duration::from_secs(PUBLISH_WAIT_SECS), EVENTS.send(entry))
                .await
                .is_err()
        {
            log::warn!("WiFi: event channel full - {} dropped", net.ssid.as_str());
        }
    }
    publish(WifiState::Scanned { count: list.len() as u8 }).await;
}

// -- Sync session --------------------------------------------------------------

/// One full sync session. Every early return tears the radio down:
/// `controller` is declared first, so it drops last - the network
/// stack and sockets die before the WiFi driver deinitializes.
async fn run_sync_session(
    wifi: p::WIFI<'_>,
    ssid: &str,
    passphrase: &str,
    tz_offset_minutes: i16,
) {
    publish(WifiState::Connecting).await;
    let (mut controller, interfaces) =
        match esp_radio::wifi::new(wifi, ControllerConfig::default()) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("WiFi: radio init failed: {:?}", e);
                publish(WifiState::Failed(WifiFailure::RadioInit)).await;
                return;
            }
        };

    // An empty passphrase means an open network; otherwise the
    // default WPA2-Personal threshold also admits WPA3 / mixed APs.
    let mut station = StationConfig::default()
        .with_ssid(ssid)
        .with_password(String::from(passphrase));
    if passphrase.is_empty() {
        station = station.with_auth_method(AuthenticationMethod::None);
    }
    if let Err(e) = controller.set_config(&WifiConfig::Station(station)) {
        log::warn!("WiFi: station config rejected: {:?}", e);
        publish(WifiState::Failed(WifiFailure::RadioInit)).await;
        return;
    }

    // Session-scoped embassy-net stack over the station interface.
    // Sockets in play: DHCP + DNS + our UDP socket.
    let mut resources: StackResources<3> = StackResources::new();
    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;
    let (stack, mut runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(dhcp_config()),
        &mut resources,
        seed,
    );

    // The stack's poll loop (`runner.run()`) never returns; it runs
    // only while this select lives - ending the session ends it.
    let work = with_timeout(
        Duration::from_secs(SYNC_BUDGET_SECS),
        sync_once(&mut controller, stack, tz_offset_minutes),
    );
    let outcome = match select(runner.run(), work).await {
        Either::First(never) => match never {},
        Either::Second(Ok(state)) => state,
        Either::Second(Err(_)) => {
            log::warn!("WiFi: session budget ({}s) exhausted", SYNC_BUDGET_SECS);
            WifiState::Failed(WifiFailure::Timeout)
        }
    };
    publish(outcome).await;
}

/// How long the DHCP client waits for an offer before sending a new
/// DISCOVER. smoltcp's default is 10 s, and that full 10 s was
/// measured on hardware 2026-08-20: the first DISCOVER goes out the
/// instant the link comes up - while the AP is still finishing with
/// the freshly associated station - and got dropped, so the session
/// sat idle until the retry, which then leased immediately. The
/// retry is the fix for a lost datagram; waiting 10 s for it is not.
/// 2 s is well clear of a healthy lease (measured in tens of ms once
/// the DISCOVER lands) and costs 5 attempts inside the same budget.
const DHCP_DISCOVER_TIMEOUT_SECS: u64 = 2;

/// DHCP client settings for a session: stock apart from the discover
/// timeout above.
fn dhcp_config() -> embassy_net::DhcpConfig {
    let mut cfg = embassy_net::DhcpConfig::default();
    cfg.retry_config.discover_timeout =
        smoltcp::time::Duration::from_secs(DHCP_DISCOVER_TIMEOUT_SECS);
    cfg
}

/// The session's actual work: associate, lease, resolve, exchange,
/// set the RTC. Returns the terminal state for the UI; the caller's
/// teardown is identical either way.
async fn sync_once(
    controller: &mut WifiController<'_>,
    stack: embassy_net::Stack<'_>,
    tz_offset_minutes: i16,
) -> WifiState {
    match controller.connect_async().await {
        Ok(info) => log::info!("WiFi: connected: {:?}", info),
        Err(e) => {
            log::warn!("WiFi: connect failed: {:?}", e);
            return WifiState::Failed(classify_connect_error(&e));
        }
    }

    if with_timeout(Duration::from_secs(LEASE_BUDGET_SECS), stack.wait_config_up())
        .await
        .is_err()
    {
        log::warn!("WiFi: no DHCP lease within {}s", LEASE_BUDGET_SECS);
        return WifiState::Failed(WifiFailure::NoLease);
    }
    match stack.config_v4() {
        Some(cfg) => {
            log::info!("WiFi: DHCP lease - ip {}", cfg.address);
        }
        None => {
            log::warn!("WiFi: link up but no IPv4 config");
            return WifiState::Failed(WifiFailure::NoLease);
        }
    }

    // Walk the fallback list: resolve each name fresh, give each
    // address NTP_ATTEMPTS tries. First timestamp wins.
    let mut unix_secs = None;
    'servers: for name in NTP_SERVERS {
        let server = match stack.dns_query(name, DnsQueryType::A).await {
            Ok(addrs) => match addrs.first().copied() {
                Some(addr) => addr,
                None => {
                    log::warn!("WiFi: DNS returned no address for {}", name);
                    continue;
                }
            },
            Err(e) => {
                log::warn!("WiFi: DNS lookup of {} failed: {:?}", name, e);
                continue;
            }
        };
        log::info!("WiFi: NTP server {} -> {}", name, server);
        for attempt in 1..=NTP_ATTEMPTS {
            if let Some(secs) = sntp_exchange(stack, server).await {
                unix_secs = Some(secs);
                break 'servers;
            }
            log::warn!(
                "WiFi: NTP attempt {}/{} at {} failed",
                attempt, NTP_ATTEMPTS, name,
            );
        }
    }
    let Some(unix_secs) = unix_secs else {
        log::warn!("WiFi: every NTP server failed");
        return WifiState::Failed(WifiFailure::NoNtp);
    };

    let (year, month, day, hour, minute, second) = civil_from_unix(unix_secs);
    let (year, month, day, hour, minute) = add_minutes(
        year,
        month,
        day,
        hour,
        minute,
        tz_offset_minutes as i32,
    );
    log::info!(
        "WiFi: NTP UTC ok -> local {:04}-{:02}-{:02} {:02}:{:02}:{:02} - RTC set",
        year, month, day, hour, minute, second,
    );
    RTC_COMMAND.signal(RtcCommand::SetTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    });
    WifiState::Synced { hour, minute }
}

/// Map the driver's connect failure onto something the UI can act
/// on: "no such network", "wrong passphrase", or "something else".
fn classify_connect_error(e: &WifiError) -> WifiFailure {
    match e {
        WifiError::Disconnected(info) => match info.reason {
            DisconnectReason::NoAccessPointFound
            | DisconnectReason::NoAccessPointFoundWithCompatibleSecurity
            | DisconnectReason::NoAccessPointFoundInAuthmodeThreshold
            | DisconnectReason::NoAccessPointFoundInRssiThreshold => WifiFailure::NoAp,
            // A wrong WPA2 passphrase surfaces as the 4-way
            // handshake timing out (the AP never completes it) or as
            // an outright auth failure.
            DisconnectReason::FourWayHandshakeTimeout
            | DisconnectReason::HandshakeTimeout
            | DisconnectReason::AuthenticationFailed
            | DisconnectReason::AuthenticationExpired
            | DisconnectReason::MicFailure => WifiFailure::AuthFailed,
            _ => WifiFailure::ConnectFailed,
        },
        WifiError::InvalidSsid | WifiError::InvalidPassword => WifiFailure::AuthFailed,
        _ => WifiFailure::ConnectFailed,
    }
}

/// One SNTP round-trip (RFC 4330, the 48-byte packet): returns the
/// server's transmit timestamp as Unix seconds, rounded to the
/// nearest second via the fraction field. No offset/delay math - a
/// single query is plenty for a wrist-watch RTC.
async fn sntp_exchange(
    stack: embassy_net::Stack<'_>,
    server: IpAddress,
) -> Option<u64> {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 128];
    let mut tx_buf = [0u8; 128];
    let mut socket =
        UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    if let Err(e) = socket.bind(0) {
        log::warn!("WiFi: UDP bind failed: {:?}", e);
        return None;
    }

    // LI = 0, version = 4, mode = 3 (client); the rest zero.
    let mut request = [0u8; 48];
    request[0] = 0x23;
    if let Err(e) = socket.send_to(&request, (server, 123)).await {
        log::warn!("WiFi: NTP send failed: {:?}", e);
        return None;
    }

    let mut response = [0u8; 48];
    let n = match with_timeout(
        Duration::from_secs(NTP_REPLY_SECS),
        socket.recv_from(&mut response),
    )
    .await
    {
        Ok(Ok((n, _meta))) => n,
        Ok(Err(e)) => {
            log::warn!("WiFi: NTP receive failed: {:?}", e);
            return None;
        }
        Err(_) => {
            log::warn!("WiFi: no NTP reply within {}s", NTP_REPLY_SECS);
            return None;
        }
    };
    if n < 48 {
        log::warn!("WiFi: NTP response truncated ({} bytes)", n);
        return None;
    }
    // Sanity: mode 4 (server) or 5 (broadcast); stratum 0 is a
    // kiss-o'-death packet.
    let mode = response[0] & 0x07;
    let stratum = response[1];
    if !(mode == 4 || mode == 5) || stratum == 0 {
        log::warn!(
            "WiFi: NTP response rejected (mode {}, stratum {})",
            mode,
            stratum
        );
        return None;
    }

    let ntp_secs =
        u32::from_be_bytes([response[40], response[41], response[42], response[43]]);
    let frac =
        u32::from_be_bytes([response[44], response[45], response[46], response[47]]);
    // NTP era handling: the 32-bit seconds counter wraps in 2036.
    // Timestamps with the high bit set are era 0 (1968-2036); clear
    // means era 1 (2036-2104), offset by exactly 2^32.
    let unix = if ntp_secs & 0x8000_0000 != 0 {
        (ntp_secs - NTP_UNIX_OFFSET) as u64
    } else {
        ntp_secs as u64 + (u32::MAX as u64 + 1 - NTP_UNIX_OFFSET as u64)
    };
    Some(unix + if frac >= 0x8000_0000 { 1 } else { 0 })
}

/// Unix seconds -> UTC calendar date/time. Days-to-civil conversion
/// per Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_unix(secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) =
        ((rem / 3600) as u8, ((rem % 3600) / 60) as u8, (rem % 60) as u8);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    (year, month, day, hour, minute, second)
}

// -- Serve session -------------------------------------------------------------
//
// A file-serving session: join the stored network, then answer HTTP
// on the LAN until the UI stops it or the budget expires. Unlike the
// other two sessions this one has no work of its own to finish - the
// user leaving the screen is what ends it.
//
// Read-only by design. There is no upload path, so the worst a
// request can do is read a file this device already shows on screen.

/// Longest a serve session may run unattended. The UI stops the
/// session when the view closes, so this only catches the case where
/// that never happens (a crash on the UI side, or a screen left open
/// in a pocket). Long enough to pull a log over a slow link, short
/// enough that a forgotten session cannot flatten the battery.
const SERVE_BUDGET_SECS: u64 = 15 * 60;

/// Listening port. Not 80: that would need a privileged port on some
/// clients' tooling and offers nothing here.
const SERVE_PORT: u16 = 8080;

/// The one directory exposed. Logs are the reason this exists - the
/// battery percent/voltage trace in particular - and confining the
/// server to a single directory is what keeps path handling trivial:
/// there are no subdirectories to walk and nothing to escape from.
const SERVE_DIR: &str = "/system/logs";

// The token alphabet and the "is this a guess?" predicate live in
// app-core (`data::FILE_SERVER_TOKEN_ALPHABET`,
// `data::looks_like_file_server_token_guess`) - this crate has no host
// tests, and that predicate decides whether the server is usable at
// all, so it belongs where it can be tested.

/// How long a single client connection may stall before it is
/// dropped, so one wedged peer cannot hold the session's only socket.
const SERVE_SOCKET_TIMEOUT_SECS: u64 = 10;

/// Bytes read from flash per chunk while streaming a file. Small on
/// purpose: every chunk is a BLOCKING flash read taken under the
/// store mutex, so this is how long the UI can be stalled at a
/// stretch. See the lock discipline on [`bus::SharedStore`].
const SERVE_CHUNK: usize = 512;

/// Generate a fresh session token.
fn make_token(rng: &mut Rng) -> [u8; FILE_SERVER_TOKEN_LEN] {
    let mut token = [0u8; FILE_SERVER_TOKEN_LEN];
    for slot in token.iter_mut() {
        let idx = (rng.random() as usize) % FILE_SERVER_TOKEN_ALPHABET.len();
        *slot = FILE_SERVER_TOKEN_ALPHABET[idx];
    }
    token
}

/// One serve session: radio up, join, lease, listen until stopped.
async fn run_serve_session(
    wifi: p::WIFI<'_>,
    ssid: &str,
    passphrase: &str,
    store: &'static bus::SharedStore,
) {
    // A stop signalled while nothing was serving must not kill this
    // session before it starts.
    bus::WIFI_STOP.reset();

    publish(WifiState::Connecting).await;
    let (mut controller, interfaces) =
        match esp_radio::wifi::new(wifi, ControllerConfig::default()) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("WiFi: radio init failed: {:?}", e);
                publish(WifiState::Failed(WifiFailure::RadioInit)).await;
                return;
            }
        };

    let mut station = StationConfig::default()
        .with_ssid(ssid)
        .with_password(String::from(passphrase));
    if passphrase.is_empty() {
        station = station.with_auth_method(AuthenticationMethod::None);
    }
    if let Err(e) = controller.set_config(&WifiConfig::Station(station)) {
        log::warn!("WiFi: station config rejected: {:?}", e);
        publish(WifiState::Failed(WifiFailure::RadioInit)).await;
        return;
    }

    // Sockets in play: DHCP + the listener. (DNS is not needed - this
    // session never resolves a name.)
    let mut resources: StackResources<3> = StackResources::new();
    let mut rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;
    let (stack, mut runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(dhcp_config()),
        &mut resources,
        seed,
    );

    let token = make_token(&mut rng);
    let outcome = match select3(
        runner.run(),
        with_timeout(
            Duration::from_secs(SERVE_BUDGET_SECS),
            serve_once(&mut controller, stack, store, token),
        ),
        bus::WIFI_STOP.wait(),
    )
    .await
    {
        Either3::First(never) => match never {},
        Either3::Second(Ok(state)) => state,
        Either3::Second(Err(_)) => {
            log::info!("WiFi: serve budget ({}s) reached", SERVE_BUDGET_SECS);
            // Worth a notification: this ending is the one the user
            // did not ask for, so without a row there is nothing to
            // explain why the server stopped answering.
            publish_server_event(FileServerEvent::TimedOut).await;
            WifiState::Idle
        }
        Either3::Third(()) => {
            log::info!("WiFi: serve stopped by UI");
            WifiState::Idle
        }
    };
    publish(outcome).await;
}

/// Associate, lease, then answer requests until cancelled. Only
/// returns on failure - a healthy session is ended from outside.
async fn serve_once(
    controller: &mut WifiController<'_>,
    stack: embassy_net::Stack<'_>,
    store: &'static bus::SharedStore,
    token: [u8; FILE_SERVER_TOKEN_LEN],
) -> WifiState {
    match controller.connect_async().await {
        Ok(info) => log::info!("WiFi: connected: {:?}", info),
        Err(e) => {
            log::warn!("WiFi: connect failed: {:?}", e);
            return WifiState::Failed(classify_connect_error(&e));
        }
    }

    if with_timeout(Duration::from_secs(LEASE_BUDGET_SECS), stack.wait_config_up())
        .await
        .is_err()
    {
        log::warn!("WiFi: no DHCP lease within {}s", LEASE_BUDGET_SECS);
        return WifiState::Failed(WifiFailure::NoLease);
    }
    let Some(cfg) = stack.config_v4() else {
        log::warn!("WiFi: link up but no IPv4 config");
        return WifiState::Failed(WifiFailure::NoLease);
    };
    let ip = cfg.address.address().octets();

    // The address and token only exist here, and the user cannot use
    // the server without both - so this publish IS the feature's
    // output, not a status nicety.
    log::info!(
        "WiFi: serving http://{}.{}.{}.{}:{}/{}/",
        ip[0], ip[1], ip[2], ip[3], SERVE_PORT,
        core::str::from_utf8(&token).unwrap_or("?"),
    );
    publish(WifiState::Serving { ip, port: SERVE_PORT, token }).await;

    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];
    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(Duration::from_secs(SERVE_SOCKET_TIMEOUT_SECS)));
        // Wait for a client, but not forever if the link is gone.
        match select(socket.accept(SERVE_PORT), link_lost(controller)).await {
            Either::First(Ok(())) => {}
            Either::First(Err(e)) => {
                log::warn!("WiFi: accept failed: {:?}", e);
                continue;
            }
            Either::Second(info) => {
                // The reason code is the whole point of logging this:
                // an AP dropping an idle station, a beacon timeout and
                // us being deauthenticated are three different bugs
                // with three different fixes.
                match info {
                    Some(d) => log::warn!(
                        "serve: link lost (reason {:?}, rssi {}) - ending session",
                        d.reason, d.rssi,
                    ),
                    None => log::warn!(
                        "serve: link lost (no reason reported) - ending session",
                    ),
                }
                publish_server_event(FileServerEvent::LinkLost).await;
                return WifiState::Failed(WifiFailure::LinkLost);
            }
        }
        let verdict = handle_request(&mut socket, store, &token).await;
        socket.close();
        // Give the peer a moment to see the FIN before the buffers are
        // reused by the next connection.
        socket.flush().await.ok();
        socket.abort();
        if verdict.is_err() {
            // Fail closed on a token guess. The token dies with the
            // session, so whatever was guessing has to start over
            // against a value that no longer exists - and the user
            // has already been told.
            log::warn!("serve: bad token - ending session");
            return WifiState::Idle;
        }
    }
}

/// Backstop poll interval for the link watch. The disconnect EVENT is
/// the primary signal; this only covers the case where the link went
/// down in the window before the wait was registered, when there is
/// no event left to receive.
const LINK_CHECK_SECS: u64 = 5;

/// Resolves when the station is no longer associated, with the reason
/// if the driver reported one.
///
/// Raced against `accept()` because an idle listener is indefinitely
/// patient: nothing about a socket that never receives anything
/// distinguishes "no one has connected yet" from "the AP dropped us
/// ten minutes ago". Without this the view keeps showing an address
/// that cannot work, which is worse than showing a failure - it looks
/// exactly like the device having hung.
///
/// Event AND poll, deliberately. `wait_for_disconnect_async` gives the
/// 802.11 reason code and the RSSI at the moment of the drop, which is
/// what tells an idle-timeout deauthentication apart from signal loss.
/// But a disconnect that happened before the wait was registered has
/// no event left to deliver, and waiting forever for it would
/// reproduce exactly the silent-dead-link bug this exists to prevent -
/// hence the parallel state poll.
async fn link_lost(
    controller: &WifiController<'_>,
) -> Option<esp_radio::wifi::DisconnectedStationInfo> {
    let poll = async {
        loop {
            embassy_time::Timer::after(Duration::from_secs(LINK_CHECK_SECS)).await;
            if !controller.is_connected() {
                return;
            }
        }
    };
    match select(controller.wait_for_disconnect_async(), poll).await {
        Either::First(Ok(info)) => Some(info),
        Either::First(Err(e)) => {
            log::warn!("serve: disconnect wait failed: {:?}", e);
            None
        }
        Either::Second(()) => None,
    }
}

/// Report a file-server event for the notification list.
async fn publish_server_event(kind: FileServerEvent) {
    let event = SystemEvent::FileServerActivity { kind };
    if EVENTS.try_send(event.clone()).is_ok() {
        return;
    }
    if with_timeout(Duration::from_secs(PUBLISH_WAIT_SECS), EVENTS.send(event))
        .await
        .is_err()
    {
        log::warn!("serve: event channel full - activity dropped");
    }
}

/// Read one request, answer it. Errors close the connection - there
/// is no keep-alive, so every request gets a fresh socket and a fresh
/// parse, and a malformed request costs nothing but that connection.
///
/// `Err(())` means the SESSION should end, not just this connection:
/// today that is a wrong-token guess, which fails closed.
async fn handle_request(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    store: &'static bus::SharedStore,
    token: &[u8; FILE_SERVER_TOKEN_LEN],
) -> Result<(), ()> {
    // Only the request line is of interest, and it is bounded: a
    // longer one is either a path we would reject anyway or a client
    // doing something we do not support.
    let mut buf = [0u8; 512];
    let mut used = 0;
    let line_end = loop {
        let Ok(n) = socket.read(&mut buf[used..]).await else { return Ok(()) };
        if n == 0 {
            return Ok(());
        }
        used += n;
        if let Some(pos) = buf[..used].windows(2).position(|w| w == b"\r\n") {
            break pos;
        }
        if used == buf.len() {
            let _ = send_status(socket, "414 URI Too Long", "uri too long\n").await;
            return Ok(());
        }
    };

    let Ok(line) = core::str::from_utf8(&buf[..line_end]) else {
        let _ = send_status(socket, "400 Bad Request", "bad request\n").await;
        return Ok(());
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        let _ = send_status(socket, "400 Bad Request", "bad request\n").await;
        return Ok(());
    };
    if method != "GET" {
        let _ = send_status(socket, "405 Method Not Allowed", "GET only\n").await;
        return Ok(());
    }

    // Token gate. Everything below this point has been authorised, so
    // this check is the ONLY thing between the network and the files.
    // Answering 404 rather than 403 keeps a wrong-token probe
    // indistinguishable from a wrong path.
    let Ok(token_str) = core::str::from_utf8(token) else { return Ok(()) };
    let Some(rest) = path
        .strip_prefix('/')
        .and_then(|p| p.strip_prefix(token_str))
    else {
        let _ = send_status(socket, "404 Not Found", "not found\n").await;
        if looks_like_file_server_token_guess(path) {
            // Shaped like a token but wrong: report it and let the
            // caller end the session. Not reported for ordinary junk
            // paths - see `looks_like_token_guess`.
            publish_server_event(FileServerEvent::BadToken).await;
            return Err(());
        }
        return Ok(());
    };

    match rest {
        "" | "/" => send_index(socket, store).await,
        _ => {
            // `strip_prefix`, not `&rest[1..]`: slicing a `str` at a
            // byte offset PANICS when that offset is not a character
            // boundary, so a path with a multi-byte character right
            // after the token would take the whole watch down - and a
            // panic here halts with interrupts disabled, which no
            // wake source can recover from. Never index a str by a
            // byte offset derived from network input.
            let Some(name) = rest.strip_prefix('/') else {
                let _ = send_status(socket, "404 Not Found", "not found\n").await;
                return Ok(());
            };
            // One directory, no subdirectories: any separator or
            // parent reference in the name is a traversal attempt, not
            // a filename we could serve.
            if name.is_empty() || name.contains('/') || name.contains("..") {
                let _ = send_status(socket, "404 Not Found", "not found\n").await;
                return Ok(());
            }
            if send_file(socket, store, name).await {
                // Only a COMPLETED download is reported. A connection
                // that died halfway is not something the user did.
                let mut sent: heapless::String<24> = heapless::String::new();
                let _ = sent.push_str(name);
                publish_server_event(FileServerEvent::Served(sent)).await;
            }
        }
    }
    Ok(())
}

/// Minimal status response with a plain-text body.
async fn send_status(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    status: &str,
    body: &str,
) -> Result<(), ()> {
    let mut head: heapless::String<128> = heapless::String::new();
    if write!(
        &mut head,
        "HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        body.len(),
    )
    .is_err()
    {
        return Err(());
    }
    socket.write_all(head.as_bytes()).await.map_err(|_| ())?;
    socket.write_all(body.as_bytes()).await.map_err(|_| ())
}

/// Directory index: one link per file in [`SERVE_DIR`].
async fn send_index(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    store: &'static bus::SharedStore,
) {
    // Collect names under the lock, render after releasing it - the
    // listing is a blocking filesystem walk and the writes below
    // await.
    let mut names: Vec<heapless::String<64>, 24> = Vec::new();
    {
        let mut guard = store.lock().await;
        guard.flash_mut().for_each_file(SERVE_DIR, |name| {
            let mut owned: heapless::String<64> = heapless::String::new();
            if owned.push_str(name).is_ok() && names.push(owned).is_ok() {
                core::ops::ControlFlow::Continue(())
            } else {
                // Out of slots: stop walking rather than silently
                // listing a truncated directory as if it were whole.
                core::ops::ControlFlow::Break(())
            }
        });
    }

    // Relative hrefs, so the token in the current URL carries over
    // without the page having to know it.
    let mut body: heapless::String<1024> = heapless::String::new();
    let _ = body.push_str("<!doctype html><meta charset=utf-8><title>uhrwerk</title><h1>logs</h1><ul>");
    for name in names.iter() {
        if write!(&mut body, "<li><a href=\"{}\">{}</a></li>", name, name).is_err() {
            break;
        }
    }
    let _ = body.push_str("</ul>");

    let mut head: heapless::String<128> = heapless::String::new();
    if write!(
        &mut head,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    )
    .is_err()
    {
        return;
    }
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let _ = socket.write_all(body.as_bytes()).await;
}

/// Stream one file out of [`SERVE_DIR`], a chunk at a time. Returns
/// true only if the whole file reached the client.
async fn send_file(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    store: &'static bus::SharedStore,
    name: &str,
) -> bool {
    let mut path: heapless::String<96> = heapless::String::new();
    if write!(&mut path, "{}/{}", SERVE_DIR, name).is_err() {
        let _ = send_status(socket, "404 Not Found", "not found\n").await;
        return false;
    }

    let size = { store.lock().await.flash_mut().file_size(&path) };
    let Some(size) = size else {
        let _ = send_status(socket, "404 Not Found", "not found\n").await;
        return false;
    };

    let mut head: heapless::String<160> = heapless::String::new();
    if write!(
        &mut head,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        size,
    )
    .is_err()
    {
        return false;
    }
    if socket.write_all(head.as_bytes()).await.is_err() {
        return false;
    }

    // Lock, read one chunk, release, write, yield. The store's methods
    // are blocking, so holding the guard across the socket write would
    // stall the UI for a whole network round trip; the yield gives the
    // render loop a slot between chunks.
    let mut offset = 0u32;
    let mut chunk = [0u8; SERVE_CHUNK];
    while offset < size {
        let read = {
            let mut guard = store.lock().await;
            guard.flash_mut().read_file_range(&path, offset, &mut chunk)
        };
        let Some(n) = read else { return false };
        if n == 0 {
            // Short read before Content-Length: the file shrank under
            // us (a log reset mid-download). Nothing honest left to
            // send - drop the connection rather than pad it.
            log::warn!("serve: {} ended early at {}/{}", path, offset, size);
            return false;
        }
        if socket.write_all(&chunk[..n]).await.is_err() {
            return false;
        }
        offset += n as u32;
        embassy_futures::yield_now().await;
    }
    true
}

