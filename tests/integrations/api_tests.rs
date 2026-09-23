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
