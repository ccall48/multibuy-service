use crate::common;
use helium_proto::Region;

const HOTSPOT_A: &str = "13QZwkEXgjE3WzWzy6DvJ1dqKsZM5s3fc4pkFpFb2yME2nRRnJv";
/// A second address, valid base58check so the validating endpoints accept it.
/// (`grpc_tests` uses an invented address for its "some other hotspot" case,
/// which is fine there — that path matches raw bytes and never validates.)
const HOTSPOT_B: &str = "112bUuQaE7j73THS9ABShHGokm46Miip9L361FSyWv7zSYn8hZWf";

/// Computed once here so the test fails loudly if name derivation ever changes.
fn animal_name(address: &str) -> String {
    multi_buy_service::deny_lists::animal_name(address)
}

#[tokio::test]
async fn deny_list_endpoints_show_configured_entries() {
    let settings = common::test_settings_with_deny_lists(
        vec![HOTSPOT_A.to_string()],
        vec!["EU868".to_string()],
    );
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let (status, body) = common::http(api, "GET", "/api/v1/deny-list", None, None).await;
    assert_eq!(status, 200);
    assert!(body.contains(HOTSPOT_A), "body was {body}");
    assert!(body.contains("EU868"), "body was {body}");
    // Denied hotspots carry their Angry Purple Tiger name alongside the address.
    assert!(body.contains(&animal_name(HOTSPOT_A)), "body was {body}");

    let (status, body) = common::http(api, "GET", "/api/v1/deny-list/hotspots", None, None).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"count\":1"), "body was {body}");

    let (status, body) = common::http(api, "GET", "/api/v1/deny-list/regions", None, None).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"count\":1"), "body was {body}");
}

#[tokio::test]
async fn added_region_takes_effect_without_restart() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
    let mut client = common::connect_client(grpc_addr).await;

    // Not denied to begin with.
    let res = common::inc(&mut client, "key1", vec![], Region::Eu868 as i32).await;
    assert!(!res.denied);

    let (status, body) = common::http(
        api,
        "POST",
        "/api/v1/deny-list/regions",
        None,
        Some(r#"{"regions":["EU868"]}"#),
    )
    .await;
    assert_eq!(status, 200, "body was {body}");
    assert!(body.contains("\"changed\":[\"EU868\"]"), "body was {body}");

    // The very next request on the live server is denied.
    let res = common::inc(&mut client, "key1", vec![], Region::Eu868 as i32).await;
    assert!(res.denied, "region added via API should deny immediately");

    // Other regions are untouched.
    let res = common::inc(&mut client, "key2", vec![], Region::As9231 as i32).await;
    assert!(!res.denied);

    // And removal takes effect just as fast.
    let (status, body) =
        common::http(api, "DELETE", "/api/v1/deny-list/regions/EU868", None, None).await;
    assert_eq!(status, 200, "body was {body}");

    let res = common::inc(&mut client, "key3", vec![], Region::Eu868 as i32).await;
    assert!(
        !res.denied,
        "region removed via API should be allowed again"
    );
}

#[tokio::test]
async fn added_hotspot_takes_effect_without_restart() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
    let mut client = common::connect_client(grpc_addr).await;

    let res = common::inc(&mut client, "key1", HOTSPOT_A.as_bytes().to_vec(), 5).await;
    assert!(!res.denied);

    let (status, body) = common::http(
        api,
        "POST",
        "/api/v1/deny-list/hotspots",
        None,
        Some(&format!(r#"{{"hotspot":"{HOTSPOT_A}"}}"#)),
    )
    .await;
    assert_eq!(status, 200, "body was {body}");

    let res = common::inc(&mut client, "key2", HOTSPOT_A.as_bytes().to_vec(), 5).await;
    assert!(res.denied, "hotspot added via API should deny immediately");

    // A different hotspot is unaffected.
    let res = common::inc(&mut client, "key3", HOTSPOT_B.as_bytes().to_vec(), 5).await;
    assert!(!res.denied);

    let (status, _) = common::http(
        api,
        "DELETE",
        &format!("/api/v1/deny-list/hotspots/{HOTSPOT_A}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);

    let res = common::inc(&mut client, "key4", HOTSPOT_A.as_bytes().to_vec(), 5).await;
    assert!(
        !res.denied,
        "hotspot removed via API should be allowed again"
    );
}

#[tokio::test]
async fn invalid_entries_are_rejected_without_partial_changes() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    // One good region, one nonsense one: nothing should be applied.
    let (status, body) = common::http(
        api,
        "POST",
        "/api/v1/deny-list/regions",
        None,
        Some(r#"{"regions":["EU868","NOT_A_REGION"]}"#),
    )
    .await;
    assert_eq!(status, 400, "body was {body}");
    assert!(body.contains("NOT_A_REGION"), "body was {body}");

    let (_, body) = common::http(api, "GET", "/api/v1/deny-list/regions", None, None).await;
    assert!(body.contains("\"count\":0"), "body was {body}");

    // A mistyped hotspot key would silently never match, so it's rejected too.
    let (status, body) = common::http(
        api,
        "POST",
        "/api/v1/deny-list/hotspots",
        None,
        Some(r#"{"hotspots":["not-a-hotspot-key"]}"#),
    )
    .await;
    assert_eq!(status, 400, "body was {body}");

    let (_, body) = common::http(api, "GET", "/api/v1/deny-list/hotspots", None, None).await;
    assert!(body.contains("\"count\":0"), "body was {body}");
}

#[tokio::test]
async fn repeated_add_and_remove_report_unchanged() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let add = || {
        common::http(
            api,
            "POST",
            "/api/v1/deny-list/regions",
            None,
            Some(r#"{"region":"US915"}"#),
        )
    };

    let (_, body) = add().await;
    assert!(body.contains("\"changed\":[\"US915\"]"), "body was {body}");

    let (_, body) = add().await;
    assert!(
        body.contains("\"unchanged\":[\"US915\"]"),
        "body was {body}"
    );
    assert!(body.contains("\"changed\":[]"), "body was {body}");

    let (_, body) =
        common::http(api, "DELETE", "/api/v1/deny-list/regions/US915", None, None).await;
    assert!(body.contains("\"changed\":[\"US915\"]"), "body was {body}");

    let (_, body) =
        common::http(api, "DELETE", "/api/v1/deny-list/regions/US915", None, None).await;
    assert!(
        body.contains("\"unchanged\":[\"US915\"]"),
        "body was {body}"
    );
}

#[tokio::test]
async fn auth_token_guards_the_api_but_not_the_page() {
    let mut settings = common::test_settings();
    settings.api.auth_token = Some("s3cret".to_string());
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let (status, _) = common::http(api, "GET", "/api/v1/deny-list", None, None).await;
    assert_eq!(status, 401, "no token should be rejected");

    let (status, _) = common::http(api, "GET", "/api/v1/deny-list", Some("wrong"), None).await;
    assert_eq!(status, 401, "wrong token should be rejected");

    let (status, _) = common::http(api, "GET", "/api/v1/deny-list", Some("s3cret"), None).await;
    assert_eq!(status, 200, "correct token should be accepted");

    // Mutations are guarded too.
    let (status, _) = common::http(
        api,
        "POST",
        "/api/v1/deny-list/regions",
        None,
        Some(r#"{"region":"US915"}"#),
    )
    .await;
    assert_eq!(status, 401);

    // The dashboard shell and health check stay reachable so the page can load
    // and prompt for a token.
    let (status, _) = common::http(api, "GET", "/", None, None).await;
    assert_eq!(status, 200);
    let (status, body) = common::http(api, "GET", "/health", None, None).await;
    assert_eq!(status, 200);
    assert_eq!(body.trim(), "ok");
}

#[tokio::test]
async fn dashboard_and_metrics_are_served() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let (status, body) = common::http(api, "GET", "/", None, None).await;
    assert_eq!(status, 200);
    assert!(body.contains("Multibuy Service"), "dashboard should render");
    assert!(
        body.contains("/api/v1/metrics"),
        "dashboard should poll the metrics endpoint"
    );

    let (status, _) = common::http(api, "GET", "/api/v1/metrics", None, None).await;
    assert_eq!(status, 200);

    let (status, body) = common::http(api, "GET", "/api/v1/info", None, None).await;
    assert_eq!(status, 200);
    assert!(body.contains(&grpc_addr.to_string()), "body was {body}");

    let (status, body) = common::http(api, "GET", "/api/v1/regions", None, None).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("US915") && body.contains("AS923_1B"),
        "body was {body}"
    );
}

#[tokio::test]
async fn hotspot_responses_carry_animal_names() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let name = animal_name(HOTSPOT_A);
    assert_eq!(name.split('-').count(), 3, "name was {name}");

    // The name comes back on the mutation that adds it...
    let (status, body) = common::http(
        api,
        "POST",
        "/api/v1/deny-list/hotspots",
        None,
        Some(&format!(r#"{{"hotspot":"{HOTSPOT_A}"}}"#)),
    )
    .await;
    assert_eq!(status, 200, "body was {body}");
    assert!(body.contains(&name), "body was {body}");

    // ...and on the listing.
    let (_, body) = common::http(api, "GET", "/api/v1/deny-list/hotspots", None, None).await;
    assert!(body.contains(&name), "body was {body}");
    assert!(body.contains(HOTSPOT_A), "body was {body}");

    // The lookup endpoint resolves a name without changing anything.
    let (status, body) = common::http(
        api,
        "GET",
        &format!("/api/v1/animal-name/{HOTSPOT_B}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains(&animal_name(HOTSPOT_B)), "body was {body}");

    let (_, body) = common::http(api, "GET", "/api/v1/deny-list/hotspots", None, None).await;
    assert!(
        !body.contains(HOTSPOT_B),
        "a name lookup must not deny anything: {body}"
    );

    // And rejects input that isn't a hotspot address.
    let (status, _) = common::http(api, "GET", "/api/v1/animal-name/nope", None, None).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn changes_survive_a_restart() {
    let store = common::temp_store_path("restart");
    let mut settings = common::test_settings_with_deny_lists(vec![], vec!["AU915".to_string()]);
    settings.deny_list_store = store.clone();

    // First run: deny a region and a hotspot, and un-deny a configured region.
    {
        let grpc_addr = common::available_port().await;
        let (shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

        let (status, body) = common::http(
            api,
            "POST",
            "/api/v1/deny-list/regions",
            None,
            Some(r#"{"region":"EU868"}"#),
        )
        .await;
        assert_eq!(status, 200, "body was {body}");
        assert!(body.contains("\"persisted\":true"), "body was {body}");

        let (_, body) = common::http(
            api,
            "POST",
            "/api/v1/deny-list/hotspots",
            None,
            Some(&format!(r#"{{"hotspot":"{HOTSPOT_A}"}}"#)),
        )
        .await;
        assert!(body.contains("\"persisted\":true"), "body was {body}");

        // Removing a region that came from settings must also stick.
        let (status, _) =
            common::http(api, "DELETE", "/api/v1/deny-list/regions/AU915", None, None).await;
        assert_eq!(status, 200);

        shutdown.trigger();
    }

    assert!(store.exists(), "store file should have been written");

    // Second run: a fresh State reading the same store.
    {
        let grpc_addr = common::available_port().await;
        let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
        let mut client = common::connect_client(grpc_addr).await;

        let (_, body) = common::http(api, "GET", "/api/v1/deny-list", None, None).await;
        assert!(
            body.contains("EU868"),
            "added region should persist: {body}"
        );
        assert!(
            body.contains(HOTSPOT_A),
            "added hotspot should persist: {body}"
        );
        assert!(
            !body.contains("AU915"),
            "a removal of a configured region should persist: {body}"
        );

        // The restored lists are live on the gRPC path, not just in the API view.
        let res = common::inc(&mut client, "k1", vec![], Region::Eu868 as i32).await;
        assert!(res.denied, "restored region should deny");
        let res = common::inc(&mut client, "k2", HOTSPOT_A.as_bytes().to_vec(), 5).await;
        assert!(res.denied, "restored hotspot should deny");
        let res = common::inc(&mut client, "k3", vec![], Region::Au915 as i32).await;
        assert!(!res.denied, "removed region should stay allowed");
    }

    std::fs::remove_file(&store).ok();
}

#[tokio::test]
async fn config_additions_still_apply_after_persisting() {
    let store = common::temp_store_path("config-add");

    // First run persists an API-added region.
    {
        let mut settings = common::test_settings();
        settings.deny_list_store = store.clone();
        let grpc_addr = common::available_port().await;
        let (shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
        let (status, _) = common::http(
            api,
            "POST",
            "/api/v1/deny-list/regions",
            None,
            Some(r#"{"region":"KR920"}"#),
        )
        .await;
        assert_eq!(status, 200);
        shutdown.trigger();
    }

    // Second run adds a region to settings: the store must not shadow it.
    {
        let mut settings = common::test_settings_with_deny_lists(vec![], vec!["IN865".to_string()]);
        settings.deny_list_store = store.clone();
        let grpc_addr = common::available_port().await;
        let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

        let (_, body) = common::http(api, "GET", "/api/v1/deny-list/regions", None, None).await;
        assert!(body.contains("KR920"), "persisted region missing: {body}");
        assert!(
            body.contains("IN865"),
            "newly configured region should apply: {body}"
        );
    }

    std::fs::remove_file(&store).ok();
}

#[tokio::test]
async fn unpersisted_changes_are_reported_as_such() {
    // Persistence off: the change still applies, but says it won't survive.
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let (status, body) = common::http(
        api,
        "POST",
        "/api/v1/deny-list/regions",
        None,
        Some(r#"{"region":"EU868"}"#),
    )
    .await;
    assert_eq!(status, 200, "the change should still be applied");
    assert!(body.contains("\"persisted\":false"), "body was {body}");
    assert!(body.contains("in memory only"), "body was {body}");

    let (_, body) = common::http(api, "GET", "/api/v1/info", None, None).await;
    assert!(body.contains("\"persistent\":false"), "body was {body}");
}

#[tokio::test]
async fn api_reports_which_entries_are_denying_traffic() {
    let settings = common::test_settings_with_deny_lists(
        vec![HOTSPOT_A.to_string()],
        vec!["EU868".to_string(), "KR920".to_string()],
    );
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
    let mut client = common::connect_client(grpc_addr).await;

    // Before any traffic, every rule reports as unused.
    let (_, body) = common::http(api, "GET", "/api/v1/deny-list/regions", None, None).await;
    assert!(body.contains("\"denied\":0"), "body was {body}");
    assert!(body.contains("\"never_matched\":2"), "body was {body}");

    // Three EU868 denials, one hotspot denial in an allowed region, one allowed.
    for i in 0..3 {
        let res = common::inc(
            &mut client,
            &format!("eu-{i}"),
            vec![],
            Region::Eu868 as i32,
        )
        .await;
        assert!(res.denied);
    }
    let res = common::inc(
        &mut client,
        "hs",
        HOTSPOT_A.as_bytes().to_vec(),
        Region::Au915 as i32,
    )
    .await;
    assert!(res.denied);
    let res = common::inc(&mut client, "ok", vec![], Region::Au915 as i32).await;
    assert!(!res.denied);

    let regions: serde_json::Value = serde_json::from_str(
        &common::http(api, "GET", "/api/v1/deny-list/regions", None, None)
            .await
            .1,
    )
    .unwrap();

    assert_eq!(regions["activity_since_start"]["denied"], 3);
    // KR920 was configured but never matched — the thing worth spotting.
    assert_eq!(regions["activity_since_start"]["never_matched"], 1);

    // Busiest first, with a timestamp on the entry that matched.
    assert_eq!(regions["regions"][0]["region"], "EU868");
    assert_eq!(regions["regions"][0]["hits"], 3);
    assert!(regions["regions"][0]["last_denied"].is_number());
    assert_eq!(regions["regions"][1]["region"], "KR920");
    assert_eq!(regions["regions"][1]["hits"], 0);
    assert!(
        regions["regions"][1]["last_denied"].is_null(),
        "an unused rule has no last-denied time"
    );

    let hotspots: serde_json::Value = serde_json::from_str(
        &common::http(api, "GET", "/api/v1/deny-list/hotspots", None, None)
            .await
            .1,
    )
    .unwrap();
    assert_eq!(hotspots["activity_since_start"]["denied"], 1);
    assert_eq!(hotspots["hotspots"][0]["address"], HOTSPOT_A);
    assert_eq!(hotspots["hotspots"][0]["hits"], 1);
    // The animal name rides along, so the busiest hotspot is identifiable.
    assert_eq!(hotspots["hotspots"][0]["name"], animal_name(HOTSPOT_A));
}

#[tokio::test]
async fn metrics_break_denials_down_by_reason_and_region() {
    let settings = common::test_settings_with_deny_lists(
        vec![HOTSPOT_A.to_string()],
        vec!["EU868".to_string()],
    );
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
    let mut client = common::connect_client(grpc_addr).await;

    // Region-only, hotspot-only, and a request matching both.
    common::inc(&mut client, "a", vec![], Region::Eu868 as i32).await;
    common::inc(
        &mut client,
        "b",
        HOTSPOT_A.as_bytes().to_vec(),
        Region::Au915 as i32,
    )
    .await;
    common::inc(
        &mut client,
        "c",
        HOTSPOT_A.as_bytes().to_vec(),
        Region::Eu868 as i32,
    )
    .await;

    // The per-entry view attributes the "both" request to each list.
    let (_, body) = common::http(api, "GET", "/api/v1/deny-list", None, None).await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["activity_since_start"]["regions"]["denied"], 2);
    assert_eq!(view["activity_since_start"]["hotspots"]["denied"], 2);
}

#[tokio::test]
async fn traffic_endpoint_counts_grpc_requests() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
    let mut client = common::connect_client(grpc_addr).await;

    for key in ["t1", "t2", "t3"] {
        common::inc(&mut client, key, vec![], Region::Us915 as i32).await;
    }

    let (status, body) = common::http(api, "GET", "/api/v1/traffic?min_gap=5", None, None).await;
    assert_eq!(status, 200, "body was {body}");
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["total"], 3, "body was {body}");
    assert_eq!(view["min_gap"], 5);
    assert_eq!(view["window_seconds"], 3600);
    assert!(view["last_request"].is_u64(), "body was {body}");
    assert!(
        view["silences"].as_array().unwrap().is_empty(),
        "body was {body}"
    );
}

#[tokio::test]
async fn connections_endpoint_records_open_and_close() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let mut client = common::connect_client(grpc_addr).await;
    common::inc(&mut client, "c1", vec![], Region::Us915 as i32).await;

    let (status, body) = common::http(api, "GET", "/api/v1/connections", None, None).await;
    assert_eq!(status, 200, "body was {body}");
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["active"], 1, "body was {body}");
    assert_eq!(view["opened_total"], 1, "body was {body}");
    assert_eq!(view["events"][0]["kind"], "opened", "body was {body}");

    // Dropping the client closes its connection; the server sees the EOF.
    drop(client);
    let mut closed = None;
    for _ in 0..50 {
        let (_, body) = common::http(api, "GET", "/api/v1/connections", None, None).await;
        let view: serde_json::Value = serde_json::from_str(&body).unwrap();
        if view["active"] == 0 {
            closed = Some(view);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let view = closed.expect("connection never recorded as closed");
    let close = &view["events"][1];
    assert_eq!(close["kind"], "closed", "view was {view}");
    // A clean client disconnect: either we read its EOF first, or hyper closed
    // the socket after its GOAWAY. Never an error, never "unresponsive".
    let reason = close["reason"].as_str().unwrap();
    assert!(
        reason == "peer closed" || reason == "closed cleanly",
        "view was {view}"
    );
    assert!(close["open_seconds"].is_u64(), "view was {view}");
}

#[tokio::test]
async fn traffic_endpoint_flags_copies_after_the_dedup_window() {
    let mut settings = common::test_settings();
    settings.lns_dedup_window = std::time::Duration::from_millis(50);
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
    let mut client = common::connect_client(grpc_addr).await;

    // Two copies inside the window, then one after it from another hotspot.
    let a = HOTSPOT_A.as_bytes().to_vec();
    let b = HOTSPOT_B.as_bytes().to_vec();
    common::inc(&mut client, "pkt", a.clone(), Region::Us915 as i32).await;
    common::inc(&mut client, "pkt", a, Region::Us915 as i32).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    common::inc(&mut client, "pkt", b, Region::Us915 as i32).await;

    let (status, body) = common::http(api, "GET", "/api/v1/traffic", None, None).await;
    assert_eq!(status, 200, "body was {body}");
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["total"], 3, "body was {body}");
    assert_eq!(view["dedup_window_ms"], 50);
    assert_eq!(view["repeat_after_ms"], 3000);
    let late: u64 = view["late"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| pair[1].as_u64().unwrap())
        .sum();
    assert_eq!(late, 1, "body was {body}");
    assert!(
        view["resends"].as_array().unwrap().is_empty(),
        "body was {body}"
    );
    assert!(
        view["slow_copies"].as_array().unwrap().is_empty(),
        "body was {body}"
    );

    // The late copy is pinned on the hotspot that sent it.
    let hotspots = view["late_hotspots"].as_array().unwrap();
    assert_eq!(hotspots.len(), 1, "body was {body}");
    assert_eq!(hotspots[0]["address"], HOTSPOT_B);
    assert_eq!(hotspots[0]["name"], animal_name(HOTSPOT_B).as_str());
    assert_eq!(hotspots[0]["late"], 1);

    // All three came from this test's one client address.
    let peers = view["peers"].as_array().unwrap();
    assert_eq!(peers.len(), 1, "body was {body}");
    assert_eq!(peers[0]["ip"], "127.0.0.1");
    assert_eq!(peers[0]["total"], 3);
}

#[tokio::test]
async fn hotspots_endpoint_keeps_per_hotspot_stats() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;
    let mut client = common::connect_client(grpc_addr).await;

    let a = HOTSPOT_A.as_bytes().to_vec();
    let b = HOTSPOT_B.as_bytes().to_vec();
    // A is first on two packets; B is second on one.
    common::inc(&mut client, "p1", a.clone(), Region::Eu868 as i32).await;
    common::inc(&mut client, "p1", b, Region::Eu868 as i32).await;
    common::inc(&mut client, "p2", a, Region::Us915 as i32).await;

    let (status, body) = common::http(api, "GET", "/api/v1/hotspots", None, None).await;
    assert_eq!(status, 200, "body was {body}");
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["total"], 2, "body was {body}");
    assert_eq!(view["persistent"], false);
    let first = &view["hotspots"][0];
    assert_eq!(
        first["address"], HOTSPOT_A,
        "busiest first; body was {body}"
    );
    assert_eq!(first["name"], animal_name(HOTSPOT_A).as_str());
    assert_eq!(first["copies"], 2);
    assert_eq!(first["copies_last_hour"], 2);
    assert_eq!(first["first"], 2);
    assert_eq!(first["regions"], serde_json::json!(["US915", "EU868"]));
    assert_eq!(first["denied_now"], false);
    assert_eq!(view["hotspots"][1]["second"], 1);

    // Search matches the animal name.
    let name = animal_name(HOTSPOT_B);
    let (_, body) = common::http(
        api,
        "GET",
        &format!("/api/v1/hotspots?q={name}"),
        None,
        None,
    )
    .await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["matched"], 1, "body was {body}");
    assert_eq!(view["hotspots"][0]["address"], HOTSPOT_B);

    // Unknown sort is rejected rather than silently ignored.
    let (status, _) = common::http(api, "GET", "/api/v1/hotspots?sort=bogus", None, None).await;
    assert_eq!(status, 400);

    // Denying a hotspot shows on its row.
    let body = format!(r#"{{"hotspots":["{HOTSPOT_A}"]}}"#);
    let (status, _) =
        common::http(api, "POST", "/api/v1/deny-list/hotspots", None, Some(&body)).await;
    assert_eq!(status, 200);
    let (_, body) = common::http(api, "GET", "/api/v1/hotspots", None, None).await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["hotspots"][0]["denied_now"], true, "body was {body}");
}

#[tokio::test]
async fn hpr_labels_can_be_set_and_removed() {
    let settings = common::test_settings();
    let grpc_addr = common::available_port().await;
    let (_shutdown, api) = common::start_server_with_api(&settings, grpc_addr).await;

    let (status, body) = common::http(
        api,
        "PUT",
        "/api/v1/hpr-labels/3.72.47.84",
        None,
        Some(r#"{"label":" Frankfurt "}"#),
    )
    .await;
    assert_eq!(status, 200, "body was {body}");
    assert!(
        body.contains(r#""3.72.47.84":"Frankfurt""#),
        "body was {body}"
    );

    let (_, body) = common::http(api, "GET", "/api/v1/hpr-labels", None, None).await;
    assert!(
        body.contains(r#""3.72.47.84":"Frankfurt""#),
        "body was {body}"
    );

    // Bad input is rejected.
    let (status, _) = common::http(
        api,
        "PUT",
        "/api/v1/hpr-labels/not-an-ip",
        None,
        Some(r#"{"label":"x"}"#),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = common::http(
        api,
        "PUT",
        "/api/v1/hpr-labels/1.2.3.4",
        None,
        Some(r#"{"label":"  "}"#),
    )
    .await;
    assert_eq!(status, 400);

    let (status, body) =
        common::http(api, "DELETE", "/api/v1/hpr-labels/3.72.47.84", None, None).await;
    assert_eq!(status, 200, "body was {body}");
    assert!(body.contains(r#""labels":{}"#), "body was {body}");
}
