//! Manual real-network probes against the user's explicit credential vault.
//! Ignored by default; pass --ignored and LIVE_HUB_VAULT_DIR to run locally.

use live_hub_rust::core::Platform;

#[test]
#[ignore = "real network + local credential vault"]
fn douyin_search_with_user_vault() {
    let root = std::env::var("LIVE_HUB_VAULT_DIR").expect("LIVE_HUB_VAULT_DIR");
    let vault =
        live_hub_rust::auth::CookieVault::open(std::path::PathBuf::from(root)).expect("open vault");
    let keyword = std::env::var("LIVE_HUB_SEARCH_KEYWORD").unwrap_or_else(|_| "花小龙".to_owned());
    let result = live_hub_rust::platforms::search_authenticated(Platform::Douyin, &keyword, &vault);
    match result {
        Ok(rows) => {
            println!("DOUYIN_SEARCH_COUNT={}", rows.len());
            for row in rows.iter().take(5) {
                println!(
                    "CAND {} {} {:?}",
                    row.name,
                    row.profile_url.as_deref().unwrap_or(""),
                    row.sec_uid.as_deref().unwrap_or("")
                );
                let codes: Vec<String> = row
                    .description
                    .chars()
                    .map(|c| format!("U+{:04X}", c as u32))
                    .collect();
                println!("DESC_CODES {}", codes.join(" "));
            }
        }
        Err(error) => println!("DOUYIN_SEARCH_ERROR={error:#}"),
    }
}
#[test]
#[ignore = "real network + local credential vault"]
fn all_platform_search_with_user_vault() {
    let root = std::env::var("LIVE_HUB_VAULT_DIR").expect("LIVE_HUB_VAULT_DIR");
    let vault =
        live_hub_rust::auth::CookieVault::open(std::path::PathBuf::from(root)).expect("open vault");
    let keyword = std::env::var("LIVE_HUB_SEARCH_KEYWORD").unwrap_or_else(|_| "陈泽".to_owned());
    match live_hub_rust::search::all(&keyword, &vault) {
        Ok(bundle) => {
            println!("ALL_COUNT={}", bundle.candidates.len());
            for outcome in &bundle.outcomes {
                println!(
                    "OUTCOME {:?} success={} candidates={} note={}",
                    outcome.platform, outcome.success, outcome.candidates, outcome.note
                );
            }
            for candidate in &bundle.candidates {
                println!(
                    "CANDIDATE {:?} name={:?} avatar={:?}",
                    candidate.platform, candidate.name, candidate.avatar_url
                );
            }
        }
        Err(error) => println!("ALL_ERROR={error:#}"),
    }
}
#[test]
#[ignore = "real network + local credential vault"]
fn douyin_verify_uses_cookie_free_public_endpoints() {
    let root = std::env::var("LIVE_HUB_VAULT_DIR").expect("LIVE_HUB_VAULT_DIR");
    let vault =
        live_hub_rust::auth::CookieVault::open(std::path::PathBuf::from(root)).expect("open vault");
    let keyword = std::env::var("LIVE_HUB_SEARCH_KEYWORD").unwrap_or_else(|_| "陈泽".to_owned());
    let candidates = live_hub_rust::platforms::search_authenticated(
        live_hub_rust::core::Platform::Douyin,
        &keyword,
        &vault,
    )
    .expect("search");
    for candidate in candidates.iter() {
        if candidate.live_status.as_deref() == Some("live") {
            println!(
                "LIVE_CANDIDATE {:?} {}",
                candidate.name,
                candidate.profile_url.as_deref().unwrap_or("")
            );
        }
    }
    for candidate in candidates.iter().take(10) {
        match live_hub_rust::platforms::verify_authenticated(candidate, &vault) {
            Ok(verified) => println!(
                "VERIFIED {:?} room={} live={:?}",
                verified.name, verified.room, verified.live_status
            ),
            Err(error) => {
                let message = format!("{error:#}");
                println!("VERIFY_ERR {:?}: {message}", candidate.name);
                assert!(
                    !message.contains("outside the selected platform"),
                    "public Douyin verify must not be blocked by the cookie-domain guard"
                );
            }
        }
    }
}
#[test]
#[ignore = "real network + local credential vault"]
fn find_and_verify_a_live_douyin_candidate() {
    let root = std::env::var("LIVE_HUB_VAULT_DIR").expect("LIVE_HUB_VAULT_DIR");
    let vault =
        live_hub_rust::auth::CookieVault::open(std::path::PathBuf::from(root)).expect("open vault");
    for keyword in ["陈泽", "花小龙", "张大仙", "旭旭宝宝", "Uzi"] {
        let candidates = match live_hub_rust::platforms::search_authenticated(
            live_hub_rust::core::Platform::Douyin,
            keyword,
            &vault,
        ) {
            Ok(candidates) => candidates,
            Err(error) => {
                println!("SEARCH_ERR {keyword}: {error:#}");
                continue;
            }
        };
        for candidate in candidates {
            if candidate.live_status.as_deref() != Some("live") {
                continue;
            }
            println!(
                "LIVE {:?} handle={:?} {}",
                candidate.name,
                candidate.account_handle,
                candidate.live_status.as_deref().unwrap_or("")
            );
            match live_hub_rust::platforms::verify_authenticated(&candidate, &vault) {
                Ok(verified) => println!(
                    "LIVE_VERIFIED {:?} room={} live={:?}",
                    verified.name, verified.room, verified.live_status
                ),
                Err(error) => println!("LIVE_VERIFY_ERR {:?}: {error:#}", candidate.name),
            }
        }
    }
}
#[test]
#[ignore = "real network + local credential vault"]
fn douyu_resolve_authenticated_vs_anonymous() {
    let root = std::env::var("LIVE_HUB_VAULT_DIR").expect("LIVE_HUB_VAULT_DIR");
    let vault =
        live_hub_rust::auth::CookieVault::open(std::path::PathBuf::from(root)).expect("open vault");
    let target = live_hub_rust::core::Target::parse("douyu:633019").expect("target");
    match live_hub_rust::platforms::resolve_authenticated(
        &target,
        &live_hub_rust::core::Settings::default(),
        &vault,
    ) {
        Ok(stream) => println!("AUTH_OK url={}", stream.url),
        Err(error) => println!("AUTH_ERR {error:#}"),
    }
    match live_hub_rust::platforms::resolve(&target, &live_hub_rust::core::Settings::default()) {
        Ok(stream) => println!("ANON_OK url={}", stream.url),
        Err(error) => println!("ANON_ERR {error:#}"),
    }
}
#[test]
#[ignore = "real network + local credential vault"]
fn list_live_douyin_favorites() {
    let root = std::env::var("LIVE_HUB_VAULT_DIR").expect("LIVE_HUB_VAULT_DIR");
    let vault =
        live_hub_rust::auth::CookieVault::open(std::path::PathBuf::from(root)).expect("open vault");
    let state_path = std::env::var("LIVE_HUB_STATE_FILE").unwrap_or_else(|_| {
        r"D:/ai_tasks/08_应用项目/live-hub_rust/.local/user-data/state.json".into()
    });
    let state: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(state_path).expect("state")).expect("json");
    let mut live = Vec::new();
    for favorite in state["favorites"].as_array().cloned().unwrap_or_default() {
        let target = &favorite["target"];
        if target["platform"].as_str() != Some("douyin") {
            continue;
        }
        let Some(room) = target["room"].as_str() else {
            continue;
        };
        let Ok(parsed) = live_hub_rust::core::Target::parse(&format!("douyin:{room}")) else {
            continue;
        };
        match live_hub_rust::room_state::check(&parsed, Some(&vault)) {
            Ok(observation) => {
                println!("DOUYIN_STATE {room} {:?}", observation.state);
                if matches!(
                    observation.state,
                    live_hub_rust::room_state::RoomState::Live
                ) {
                    live.push(room.to_owned());
                }
            }
            Err(error) => println!("DOUYIN_STATE_ERR {room}: {error:#}"),
        }
    }
    println!("LIVE_ROOMS {:?}", live);
}
