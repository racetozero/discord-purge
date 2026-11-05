//! Shared selection contracts: scope, strict bounds, ownership, and browse routes.
use axum::{
    Json, Router,
    extract::{Request, State},
    response::IntoResponse,
};
use discord_purge_core::api::DiscordClient;
use discord_purge_core::filter::Filter;
use discord_purge_core::search::Target;
use discord_purge_core::selection::{self, MessageRange, MessageRef, Selection};
use discord_purge_core::{Source, prepare};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn package_selection_has_strict_bounds_and_exact_cross_channel_sets() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/package.zip");
    let client = DiscordClient::with_base("", "http://127.0.0.1:1").unwrap();
    let catalog = selection::catalog(&client, Some(&path)).await.unwrap();
    assert_eq!(catalog.guilds[0].id, "900");
    assert_eq!(catalog.dms[0].id, "222");
    let all = discord_purge_core::package::load(&path, &Filter::default()).unwrap();
    let expected: HashSet<_> = all
        .channels
        .iter()
        .flat_map(|c| c.messages.iter().map(move |m| (c.id.clone(), m.id.clone())))
        .collect();
    let history = selection::messages(&client, Some(&path), "111", None)
        .await
        .unwrap();
    let anchor = MessageRef {
        channel_id: "111".into(),
        message_id: history[history.len() / 2].id.clone(),
    };
    let boundary = anchor.message_id.parse::<u64>().unwrap();
    let targets = vec![
        Target::Guild {
            guild_id: "900".into(),
            channel: None,
        },
        Target::Guild {
            guild_id: "900".into(),
            channel: Some("111".into()),
        },
        Target::Dm {
            channel_id: "222".into(),
        },
        Target::Dm {
            channel_id: "222".into(),
        },
    ];
    for range in [
        MessageRange::All,
        MessageRange::Older {
            anchor: anchor.clone(),
        },
        MessageRange::Newer {
            anchor: anchor.clone(),
        },
    ] {
        let selection = Selection {
            targets: targets.clone(),
            range: range.clone(),
            include_pinned: false,
        };
        let run = prepare(
            &client,
            &Source::Selected {
                selection,
                package: Some(path.clone()),
            },
            &Filter::default(),
        )
        .await
        .unwrap();
        let actual: HashSet<_> = run
            .jobs
            .iter()
            .flat_map(|j| {
                j.items
                    .iter()
                    .map(|m| (j.channel_id.clone(), m.to_string()))
            })
            .collect();
        let expected: HashSet<_> = expected
            .iter()
            .filter(|(_, id)| match range {
                MessageRange::Older { .. } => id.parse::<u64>().unwrap() < boundary,
                MessageRange::Newer { .. } => id.parse::<u64>().unwrap() > boundary,
                _ => true,
            })
            .cloned()
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(run.total, actual.len());
    }
    let dm = selection::messages(&client, Some(&path), "222", None)
        .await
        .unwrap();
    let refs = vec![
        anchor.clone(),
        MessageRef {
            channel_id: "222".into(),
            message_id: dm[0].id.clone(),
        },
        anchor.clone(),
    ];
    let selection = Selection {
        targets: targets.clone(),
        range: MessageRange::Only { messages: refs },
        include_pinned: false,
    };
    let run = prepare(
        &client,
        &Source::Selected {
            selection,
            package: Some(path.clone()),
        },
        &Filter::default(),
    )
    .await
    .unwrap();
    assert_eq!(run.total, 2);
    assert_eq!(run.jobs.len(), 2);
    let invalid = Selection {
        targets: vec![Target::Dm {
            channel_id: "222".into(),
        }],
        range: MessageRange::Older { anchor },
        include_pinned: false,
    };
    assert!(
        prepare(
            &client,
            &Source::Selected {
                selection: invalid,
                package: Some(path)
            },
            &Filter::default()
        )
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("outside")
    );
}

type Seen = Arc<Mutex<Vec<String>>>;
async fn fixture(State(seen): State<Seen>, request: Request) -> axum::response::Response {
    assert_eq!(
        request.method(),
        "GET",
        "browse and preview must never delete"
    );
    assert_eq!(request.headers()["authorization"], "test-token");
    let path = request.uri().path();
    let query = request.uri().query().unwrap_or("");
    seen.lock().unwrap().push(format!("{path}?{query}"));
    let params = axum::extract::Query::<std::collections::HashMap<String, String>>::try_from_uri(
        request.uri(),
    )
    .unwrap()
    .0;
    let value = match path {
        "/v9/users/@me" => json!({"id":"7","username":"owner"}),
        "/v9/users/@me/guilds" => {
            assert_eq!(params.get("limit").map(String::as_str), Some("200"));
            if params.contains_key("after") {
                assert_eq!(params["after"], "1199");
                json!([{"id":"2000","name":"Last server"}])
            } else {
                Value::Array(
                    (1000..1200)
                        .map(|id| json!({"id":id.to_string(),"name":format!("Server {id}")}))
                        .collect(),
                )
            }
        }
        "/v9/users/@me/channels" => {
            json!([{"id":"40","type":1,"recipients":[{"id":"8","username":"friend"}]},{"id":"41","type":3,"name":"Group DM"}])
        }
        "/v9/guilds/1000/channels" => {
            json!([{"id":"10","type":0,"name":"general"},{"id":"99","type":4,"name":"category"}])
        }
        "/v9/channels/10" => json!({"id":"10","type":0,"guild_id":"1000","name":"general"}),
        "/v9/channels/40" => json!({"id":"40","type":1,"name":"DM"}),
        "/v9/channels/50" => json!({"id":"50","type":0,"guild_id":"2000","name":"outside"}),
        "/v9/channels/10/messages" => {
            if let Some(id) = params.get("around") {
                assert_eq!(params["limit"], "1");
                json!([{"id":id,"channel_id":"10","content":"selected","pinned":id=="31","author":{"id":if id=="32"{"8"}else{"7"},"username":"author"}}])
            } else {
                assert_eq!(params["limit"], "50");
                if params.contains_key("before") {
                    assert_eq!(params["before"], "30");
                    json!([{"id":"29","channel_id":"10","content":"older","pinned":false,"author":{"id":"7","username":"owner"}}])
                } else {
                    json!([{"id":"30","channel_id":"10","content":"mine","pinned":false,"author":{"id":"7","username":"owner"}}])
                }
            }
        }
        "/v9/guilds/1000/messages/search" | "/v9/channels/40/messages/search" => {
            assert_eq!(params["author_id"], "7");
            json!({"messages":[],"total_results":0})
        }
        _ => panic!("unexpected fixture route: {path}"),
    };
    Json(value).into_response()
}
#[tokio::test]
async fn live_catalog_history_and_selection_obey_the_requested_scope() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = DiscordClient::with_base(
        "test-token",
        format!("http://{}", listener.local_addr().unwrap()),
    )
    .unwrap();
    let app = Router::new().fallback(fixture).with_state(seen.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let catalog = selection::catalog(&client, None).await.unwrap();
    assert_eq!(catalog.guilds.len(), 201);
    assert_eq!(catalog.dms.len(), 2);
    let channels = selection::channels(&client, None, &["1000".into()])
        .await
        .unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].id, "10");
    assert_eq!(
        selection::messages(&client, None, "10", None)
            .await
            .unwrap()[0]
            .id,
        "30"
    );
    assert_eq!(
        selection::messages(&client, None, "10", Some("30"))
            .await
            .unwrap()[0]
            .id,
        "29"
    );
    let target = Target::Guild {
        guild_id: "1000".into(),
        channel: Some("10".into()),
    };
    let refs = vec![
        MessageRef {
            channel_id: "10".into(),
            message_id: "30".into(),
        },
        MessageRef {
            channel_id: "10".into(),
            message_id: "31".into(),
        },
        MessageRef {
            channel_id: "10".into(),
            message_id: "30".into(),
        },
    ];
    let selection = Selection {
        targets: vec![target.clone()],
        range: MessageRange::Only { messages: refs },
        include_pinned: false,
    };
    let run = prepare(
        &client,
        &Source::Selected {
            selection,
            package: None,
        },
        &Filter::default(),
    )
    .await
    .unwrap();
    assert_eq!(run.total, 1);
    assert_eq!(run.jobs[0].items[0], 30);
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with("/v9/channels/10/messages?around=30&"))
            .count(),
        1,
        "duplicate selections must not add network checks"
    );
    for range in [
        MessageRange::Older {
            anchor: MessageRef {
                channel_id: "10".into(),
                message_id: "30".into(),
            },
        },
        MessageRange::Newer {
            anchor: MessageRef {
                channel_id: "10".into(),
                message_id: "30".into(),
            },
        },
    ] {
        let selection = Selection {
            targets: vec![target.clone()],
            range,
            include_pinned: false,
        };
        prepare(
            &client,
            &Source::Selected {
                selection,
                package: None,
            },
            &Filter::default(),
        )
        .await
        .unwrap();
    }
    let queries: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|q| q.contains("/search?"))
        .cloned()
        .collect();
    assert!(queries.iter().any(|q| q.contains("max_id=30")));
    assert!(queries.iter().any(|q| q.contains("min_id=30")));
    for (channel, message, expected) in [("10", "32", "another account"), ("50", "30", "outside")] {
        let selection = Selection {
            targets: vec![target.clone()],
            range: MessageRange::Only {
                messages: vec![MessageRef {
                    channel_id: channel.into(),
                    message_id: message.into(),
                }],
            },
            include_pinned: false,
        };
        assert!(
            prepare(
                &client,
                &Source::Selected {
                    selection,
                    package: None
                },
                &Filter::default()
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains(expected)
        );
    }
    server.abort();
}

#[tokio::test]
async fn channels_in_one_server_share_one_search() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = DiscordClient::with_base(
        "test-token",
        format!("http://{}", listener.local_addr().unwrap()),
    )
    .unwrap();
    let app = Router::new().fallback(fixture).with_state(seen.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let channel = |id: &str| Target::Guild {
        guild_id: "1000".into(),
        channel: Some(id.into()),
    };
    let selection = Selection {
        targets: vec![
            channel("11"),
            channel("10"),
            Target::Dm {
                channel_id: "40".into(),
            },
        ],
        range: MessageRange::All,
        include_pinned: false,
    };
    prepare(
        &client,
        &Source::Selected {
            selection,
            package: None,
        },
        &Filter::default(),
    )
    .await
    .unwrap();
    let searches: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|q| q.contains("/search?"))
        .cloned()
        .collect();
    assert_eq!(searches.len(), 2, "{searches:?}");
    assert!(searches[0].starts_with("/v9/guilds/1000/messages/search?"));
    assert!(searches[0].contains("channel_id=10&channel_id=11&"));
    assert!(searches[1].starts_with("/v9/channels/40/messages/search?"));
    server.abort();
}
