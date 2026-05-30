use copro_agent::runtime::StopSignal;

#[test]
fn stop_signal_tracks_requested_state_across_clones() {
    let signal = StopSignal::new();
    let clone = signal.clone();

    assert!(!signal.is_requested());
    clone.request_stop();
    assert!(signal.is_requested());
    signal.clear();
    assert!(!clone.is_requested());
}
