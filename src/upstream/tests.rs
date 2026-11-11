use super::*;

#[test]
fn errno_messages() {
    let e = std::io::Error::from_raw_os_error(111);
    assert_eq!(go_errno_message(&e), "connection refused");
    let e = std::io::Error::from_raw_os_error(2);
    assert_eq!(go_errno_message(&e), "no such file or directory");
}

#[test]
fn labels() {
    assert_eq!(parse_label("").unwrap(), (0, String::new()));
    assert_eq!(
        parse_label("sd-12-3-abc-d").unwrap(),
        (12, "abc-d".to_owned())
    );
    assert!(parse_label("sd-x-1-a").is_err());
    assert!(parse_label("xx-1-1-a").is_err());
    assert!(parse_label("sd-1").is_err());
}

#[test]
fn registration_errors() {
    let e = RegistrationError(msg!(ERR_PASSWDMISMATCH, "*", "Password incorrect"));
    assert!(!e.temporary());
    assert_eq!(
        e.to_string(),
        "registration error (464): Password incorrect"
    );
    let e = RegistrationError(msg!("FAIL", "*", "ACCOUNT_REQUIRED", "Login first"));
    assert!(!e.temporary());
    assert!(RegistrationError(msg!(ERR_NICKNAMEINUSE, "*", "x", "in use")).temporary());
}

#[test]
fn caps_requested() {
    let mut caps = CapRegistry::new();
    for c in ["batch", "echo-message", "labeled-response", "foo"] {
        caps.available.insert(c.to_owned(), String::new());
    }
    assert_eq!(
        caps_to_request(&caps),
        ["batch", "labeled-response", "echo-message"]
    );

    // echo-message requires labeled-response
    caps.del("labeled-response");
    assert_eq!(caps_to_request(&caps), ["batch"]);
    caps.set_enabled("echo-message", true);
    assert_eq!(caps_to_request(&caps), ["batch", "-echo-message"]);

    // Unsupported caps are never requested
    let mut caps = CapRegistry::new();
    caps.available
        .insert("labeled-response".to_owned(), String::new());
    assert_eq!(caps_to_request(&caps), ["labeled-response"]);
}

#[test]
fn cap_ls_continuation() {
    let params = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(registration::cap_ls_continued(&params(&["*", "a b"])));
    assert!(!registration::cap_ls_continued(&params(&["a b"])));
    // The marker is looked up after the subcommand, not in the nick
    assert!(!registration::cap_ls_continued(&params(&["x", "a b"])));
}

#[test]
fn whox_fields() {
    let uu = UpstreamUser {
        nickname: "n".into(),
        username: "u".into(),
        ..Default::default()
    };
    assert!(uu.has_whox_fields("tnu"));
    assert!(!uu.has_whox_fields("nr"));
}
