use kallip_testkit::wait_for;
use std::time::Duration;

// Pins the wait_for! panic message shape: the Debug slot renders the
// duration and the plain slot renders the label. Swapping the macro's
// argument order moves 60ms after the label, so the expected substring
// in the attribute below no longer matches and this test goes red.
#[tokio::test]
#[should_panic(expected = "timed out after 60ms waiting for probe-label")]
async fn timeout_panic_names_duration_then_label() {
    wait_for!(Duration::from_millis(60), "probe-label", false);
}
