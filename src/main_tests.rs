use super::{ServeEnvironment, resolve_serve_address};

fn environment(
    host: Option<&str>,
    ironcrew_port: Option<&str>,
    platform_port: Option<&str>,
) -> ServeEnvironment {
    ServeEnvironment {
        host: host.map(str::to_owned),
        ironcrew_port: ironcrew_port.map(str::to_owned),
        platform_port: platform_port.map(str::to_owned),
    }
}

#[test]
fn serve_defaults_remain_local() {
    let address = resolve_serve_address(None, None, environment(None, None, None)).unwrap();
    assert_eq!(address, ("127.0.0.1".to_owned(), 3000));
}

#[test]
fn railway_port_binds_all_interfaces() {
    let address =
        resolve_serve_address(None, None, environment(None, None, Some("48123"))).unwrap();
    assert_eq!(address, ("0.0.0.0".to_owned(), 48123));
}

#[test]
fn ironcrew_environment_takes_precedence_over_platform_port() {
    let address = resolve_serve_address(
        None,
        None,
        environment(Some("::"), Some("4100"), Some("48123")),
    )
    .unwrap();
    assert_eq!(address, ("::".to_owned(), 4100));
}

#[test]
fn explicit_arguments_take_precedence_over_environment() {
    let address = resolve_serve_address(
        Some("127.0.0.2".to_owned()),
        Some(4200),
        environment(Some("::"), Some("4100"), Some("48123")),
    )
    .unwrap();
    assert_eq!(address, ("127.0.0.2".to_owned(), 4200));
}

#[test]
fn invalid_ironcrew_port_fails_loudly() {
    let error = resolve_serve_address(
        None,
        None,
        environment(None, Some("not-a-port"), Some("48123")),
    )
    .unwrap_err();
    assert!(error.to_string().contains("IRONCREW_PORT"));
}

#[test]
fn invalid_platform_port_fails_loudly() {
    let error =
        resolve_serve_address(None, None, environment(None, None, Some("70000"))).unwrap_err();
    assert!(error.to_string().contains("PORT"));
}

#[test]
fn zero_port_is_rejected() {
    let error = resolve_serve_address(None, Some(0), environment(None, None, None)).unwrap_err();
    assert!(error.to_string().contains("--port"));
}
