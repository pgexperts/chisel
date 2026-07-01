// tests/public_key_api.rs — Integration test proving Key/Argon2Params are
// reachable from the crate's public surface and that Options builders work
// without naming any `crate::crypto` internal path.

use chisel::{Argon2Params, Key, Options};

/// Construct Keys and Options via the public API only — this test fails to
/// compile if Key, Argon2Params, or the builders are not part of the public
/// surface.
#[test]
fn key_and_options_public_api_compiles() {
    let raw = Key::Raw(zeroize::Zeroizing::new(vec![0xABu8; 32]));
    let o = Options::default().encryption_key(raw);
    assert!(o.encryption_key.is_some());
    assert!(o.argon2_params.is_none());
}

#[test]
fn passphrase_key_and_argon2_params_public_api() {
    let pass = Key::Passphrase(zeroize::Zeroizing::new("hunter2".to_string()));
    let params = Argon2Params {
        m_cost: 32768,
        t_cost: 3,
        p_cost: 1,
    };
    let o = Options::default()
        .encryption_key(pass)
        .argon2_params(params);
    assert!(matches!(o.encryption_key, Some(Key::Passphrase(_))));
    let p = o.argon2_params.unwrap();
    assert_eq!(p.m_cost, 32768);
}

#[test]
fn options_default_has_no_encryption() {
    let o = Options::default();
    assert!(o.encryption_key.is_none());
    assert!(o.argon2_params.is_none());
}
