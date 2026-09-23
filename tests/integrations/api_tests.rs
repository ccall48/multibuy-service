use crate::common;
use helium_proto::Region;

const HOTSPOT_A: &str = "13QZwkEXgjE3WzWzy6DvJ1dqKsZM5s3fc4pkFpFb2yME2nRRnJv";
const HOTSPOT_B: &str = "11z69eJ3czc92k6snrfR1ENqbHP9bovzR4RNiB9qTDs4JDYiY3R";

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
