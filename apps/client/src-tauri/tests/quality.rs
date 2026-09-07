use locast_client_lib::net::state::{ConnectionQuality, ConnectionState};

#[test]
fn connection_quality_good_below_100ms() {
    assert_eq!(ConnectionQuality::from_rtt_ms(0), ConnectionQuality::Good);
    assert_eq!(ConnectionQuality::from_rtt_ms(50), ConnectionQuality::Good);
    assert_eq!(ConnectionQuality::from_rtt_ms(99), ConnectionQuality::Good);
}

#[test]
fn connection_quality_fair_between_100_and_300ms() {
    assert_eq!(ConnectionQuality::from_rtt_ms(100), ConnectionQuality::Fair);
    assert_eq!(ConnectionQuality::from_rtt_ms(200), ConnectionQuality::Fair);
    assert_eq!(ConnectionQuality::from_rtt_ms(300), ConnectionQuality::Fair);
}

#[test]
fn connection_quality_poor_above_300ms() {
    assert_eq!(ConnectionQuality::from_rtt_ms(301), ConnectionQuality::Poor);
    assert_eq!(ConnectionQuality::from_rtt_ms(500), ConnectionQuality::Poor);
    assert_eq!(
        ConnectionQuality::from_rtt_ms(1000),
        ConnectionQuality::Poor
    );
}

#[test]
fn connection_state_has_rtt_ms_field() {
    let state = ConnectionState::for_url("ws://example.test/ws");
    assert!(state.rtt_ms.is_none());
}

#[test]
fn connection_state_rtt_ms_is_accessible_via_specta_bindings() {
    use specta::Type;
    fn assert_type<T: Type>() {}
    assert_type::<ConnectionState>();
    assert_type::<ConnectionQuality>();
}
