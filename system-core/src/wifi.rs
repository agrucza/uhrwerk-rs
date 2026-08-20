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

use app_core::data::{WifiFailure, WifiNetwork, WifiState, MAX_WIFI_NETWORKS};
use app_core::events::SystemEvent;
use embassy_futures::select::{select, Either};
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
const NTP_SERVERS: &[&str] =
    &["pool.ntp.org", "time.cloudflare.com", "time.google.com"];

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
pub async fn wifi_task(mut wifi: p::WIFI<'static>) {
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
        embassy_net::Config::dhcpv4(Default::default()),
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
