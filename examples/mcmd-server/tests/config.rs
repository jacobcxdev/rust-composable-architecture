use std::time::Duration;

use rca_example_mcmd_server::config::{ConfigError, FloorConfig, ServerConfig, TableConfig};

fn server(id: &str, java_ports: &[u16], bedrock_ports: &[u16]) -> ServerConfig {
    ServerConfig {
        id: id.into(),
        proxy_host: "0.0.0.0".into(),
        java_ports: java_ports.into(),
        bedrock_ports: bedrock_ports.into(),
    }
}

fn table(id: &str, servers: Vec<ServerConfig>) -> TableConfig {
    TableConfig {
        id: id.into(),
        servers,
        inactivity_timeout: Duration::from_secs(30),
    }
}

#[test]
fn all_invalid_categories_preserve_context_and_configuration() {
    let mut zero_timeout = table("idle", vec![server("s", &[], &[])]);
    zero_timeout.inactivity_timeout = Duration::ZERO;
    let cases = [
        (
            vec![table("t", vec![server("a", &[], &[])]), table("t", vec![])],
            ConfigError::DuplicateTableId("t".into()),
            "duplicate table id: t",
        ),
        (
            vec![table("empty", vec![])],
            ConfigError::EmptyTable("empty".into()),
            "table empty has no servers",
        ),
        (
            vec![zero_timeout],
            ConfigError::ZeroInactivityTimeout("idle".into()),
            "table idle has inactivity_timeout=0",
        ),
        (
            vec![
                table("a", vec![server("same", &[], &[])]),
                table("b", vec![server("same", &[], &[])]),
            ],
            ConfigError::DuplicateServerId("same".into()),
            "duplicate server id: same",
        ),
        (
            vec![table(
                "t",
                vec![server("s", &[], &[]), server("s", &[], &[])],
            )],
            ConfigError::DuplicateServerIdInTable {
                table_id: "t".into(),
                server_id: "s".into(),
            },
            "table t contains duplicate server id: s",
        ),
        (
            vec![table("t", vec![server("s", &[25565], &[0])])],
            ConfigError::InvalidPort {
                table_id: "t".into(),
                server_id: "s".into(),
                port: 0,
            },
            "table t server s has invalid port: 0",
        ),
        (
            vec![table("t", vec![server("s", &[25565], &[25565])])],
            ConfigError::DuplicatePort {
                table_id: "t".into(),
                server_id: "s".into(),
                port: 25565,
            },
            "table t server s repeats port: 25565",
        ),
    ];
    for (tables, expected, diagnostic) in cases {
        let config = FloorConfig { tables };
        let before = config.clone();
        let error = config.validate().unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(error.to_string(), diagnostic);
        assert_eq!(
            config, before,
            "validation must not normalise or mutate input"
        );
    }
}

#[test]
fn valid_floor_keeps_metadata_and_allows_shared_ports_across_servers() {
    let config = FloorConfig {
        tables: vec![
            table(
                "one",
                vec![
                    server("a", &[25565], &[19132]),
                    server("b", &[25565], &[19132]),
                ],
            ),
            table("two", vec![server("c", &[25565], &[19132])]),
        ],
    };
    let before = config.clone();
    assert_eq!(config.validate(), Ok(()));
    assert_eq!(config, before);
    assert_eq!(config.tables[0].inactivity_timeout, Duration::from_secs(30));
    assert_eq!(config.tables[0].servers[0].proxy_host, "0.0.0.0");
}

#[test]
fn empty_floor_and_empty_port_lists_remain_valid() {
    assert_eq!(FloorConfig { tables: vec![] }.validate(), Ok(()));
    assert_eq!(table("t", vec![server("s", &[], &[])]).validate(), Ok(()));
}

#[test]
fn duplicate_ports_within_either_protocol_are_rejected() {
    for config in [
        server("s", &[25565, 25565], &[]),
        server("s", &[], &[25565, 25565]),
    ] {
        assert_eq!(
            config.validate("t"),
            Err(ConfigError::DuplicatePort {
                table_id: "t".into(),
                server_id: "s".into(),
                port: 25565
            })
        );
    }
}

#[test]
fn table_shape_checks_precede_server_checks() {
    let mut config = table("t", vec![]);
    config.inactivity_timeout = Duration::ZERO;
    assert_eq!(config.validate(), Err(ConfigError::EmptyTable("t".into())));
    config.servers = vec![server("s", &[0], &[]), server("s", &[], &[])];
    assert_eq!(
        config.validate(),
        Err(ConfigError::ZeroInactivityTimeout("t".into()))
    );
}

#[test]
fn table_traversal_checks_each_id_then_its_ports() {
    let duplicate = ConfigError::DuplicateServerIdInTable {
        table_id: "t".into(),
        server_id: "s".into(),
    };
    assert_eq!(
        table("t", vec![server("s", &[], &[]), server("s", &[0], &[])]).validate(),
        Err(duplicate)
    );
    assert_eq!(
        table("t", vec![server("s", &[0], &[]), server("s", &[], &[])]).validate(),
        Err(ConfigError::InvalidPort {
            table_id: "t".into(),
            server_id: "s".into(),
            port: 0
        })
    );
}

#[test]
fn floor_validates_whole_table_before_global_server_uniqueness() {
    let config = FloorConfig {
        tables: vec![
            table("a", vec![server("same", &[], &[])]),
            table(
                "b",
                vec![server("same", &[], &[]), server("bad", &[0], &[])],
            ),
        ],
    };
    assert_eq!(
        config.validate(),
        Err(ConfigError::InvalidPort {
            table_id: "b".into(),
            server_id: "bad".into(),
            port: 0
        })
    );
    let earlier_invalid = FloorConfig {
        tables: vec![table("t", vec![]), table("t", vec![])],
    };
    assert_eq!(
        earlier_invalid.validate(),
        Err(ConfigError::EmptyTable("t".into()))
    );
}

#[test]
fn java_then_bedrock_order_determines_first_port_error() {
    assert_eq!(
        server("s", &[7, 7], &[0]).validate("t"),
        Err(ConfigError::DuplicatePort {
            table_id: "t".into(),
            server_id: "s".into(),
            port: 7
        })
    );
    assert_eq!(
        server("s", &[0, 7, 7], &[]).validate("t"),
        Err(ConfigError::InvalidPort {
            table_id: "t".into(),
            server_id: "s".into(),
            port: 0
        })
    );
}
