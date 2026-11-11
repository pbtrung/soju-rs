//! Authentication regression scenarios.

use super::*;

pub(super) async fn sasl_errors() {
    let env = setup(db_store()).await;

    // AUTHENTICATE needs the sasl capability
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    let m = dc.expect("904").await;
    assert_eq!(
        m.params[1],
        "AUTHENTICATE requires the \"sasl\" capability to be enabled"
    );

    // Invalid base64 fails the exchange but keeps the connection
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("CAP", "LS", "302"));
    read_cap_ls(&mut dc).await;
    dc.send(msg!("CAP", "REQ", "sasl"));
    dc.expect("CAP").await;
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    let m = dc.expect("AUTHENTICATE").await;
    assert_eq!(m.params, vec!["+"]);
    dc.send(msg!("AUTHENTICATE", "!!!!"));
    let m = dc.expect("904").await;
    assert_eq!(m.params[1], "Invalid base64-encoded response");

    // A new exchange still works
    use base64::Engine;
    let creds = format!("\0{TEST_USERNAME}\0{TEST_PASSWORD}");
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    dc.expect("AUTHENTICATE").await;
    dc.send(msg!(
        "AUTHENTICATE",
        base64::engine::general_purpose::STANDARD.encode(creds)
    ));
    read_until(&mut dc, "903").await;

    // Repeated wrong passwords close the connection
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("CAP", "LS", "302"));
    read_cap_ls(&mut dc).await;
    dc.send(msg!("CAP", "REQ", "sasl"));
    dc.expect("CAP").await;
    let wrong = format!("\0{TEST_USERNAME}\0wrong");
    for attempt in 1..=3 {
        dc.send(msg!("AUTHENTICATE", "PLAIN"));
        dc.expect("AUTHENTICATE").await;
        dc.send(msg!(
            "AUTHENTICATE",
            base64::engine::general_purpose::STANDARD.encode(&wrong)
        ));
        dc.expect("904").await;
        if attempt < 3 {
            // Still open
            dc.send(msg!("CAP", "LIST"));
            dc.expect("CAP").await;
        }
    }
    let m = dc.expect("ERROR").await;
    assert_eq!(m.params[0], "Too many authentication failures");
    env.srv.shutdown().await;
}

pub(super) async fn sasl_chunk_boundaries() {
    use base64::Engine;
    let username = "u".repeat(300);
    let env = setup_with(db_store(), |_, user| user.username = username.clone()).await;
    let mut dc = connect_downstream(&env.srv);
    dc.send(msg!("CAP", "LS", "302"));
    read_cap_ls(&mut dc).await;
    dc.send(msg!("CAP", "REQ", "sasl"));
    dc.expect("CAP").await;
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    dc.expect("AUTHENTICATE").await;
    dc.send(msg!("AUTHENTICATE", "A".repeat(404)));
    dc.expect("905").await;

    // A valid exchange after a rejected chunk proves the state was reset.
    // A long username makes the base64 cross the 400-byte boundary.
    dc.send(msg!("AUTHENTICATE", "PLAIN"));
    dc.expect("AUTHENTICATE").await;
    let creds = format!("\0{username}\0{TEST_PASSWORD}");
    let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
    assert!(encoded.len() > 400);
    dc.send(msg!("AUTHENTICATE", &encoded[..400]));
    // CAP LIST is independent and proves the first chunk didn't end SASL.
    dc.send(msg!("CAP", "LIST"));
    dc.expect("CAP").await;
    dc.send(msg!("AUTHENTICATE", &encoded[400..]));
    read_until(&mut dc, "903").await;
    env.srv.shutdown().await;
}

pub(super) async fn disabled_admin_impersonation() {
    use base64::Engine;
    let env = setup_with(
        MsgStore {
            driver: MsgStoreDriver::Db,
            source: String::new(),
        },
        |db, _| {
            let mut admin = database::User::new("admin");
            admin.admin = true;
            admin.enabled = false;
            admin.set_password("secret").unwrap();
            db.store_user(&mut admin).unwrap();
        },
    )
    .await;
    for enabled in [false, true] {
        let mut admin = env.srv.db.get_user("admin").unwrap().unwrap();
        admin.enabled = enabled;
        env.srv.db.store_user(&mut admin).unwrap();
        let mut dc = connect_downstream(&env.srv);
        dc.send(msg!("CAP", "LS", "302"));
        dc.send(msg!("CAP", "REQ", "sasl"));
        dc.send(msg!("AUTHENTICATE", "PLAIN"));
        let payload = format!("{TEST_USERNAME}\0admin\0secret");
        dc.send(msg!(
            "AUTHENTICATE",
            base64::engine::general_purpose::STANDARD.encode(payload)
        ));
        loop {
            let m = dc.read().await;
            if m.command == "903" || m.command == "904" {
                assert_eq!(m.command, if enabled { "903" } else { "904" });
                break;
            }
        }
        if enabled {
            dc.send(msg!("NICK", "test"));
            dc.send(msg!("USER", TEST_USERNAME, "0", "*", "test"));
            dc.send(msg!("CAP", "END"));
            read_until(&mut dc, "001").await;
        }
        dc.conn.close();
    }
    env.srv.shutdown().await;
}
