//! `Session::connect` by profile name. It must build a session without touching the network, and
//! pick the region from the hint first. These run with no AWS files around (the container has
//! no ~/.aws), so the fallback region is us-east-1.

use reses::aws_profile::Profile;
use reses::tui::Session;

#[test]
fn connect_takes_a_profile_name() {
    let s = Session::connect("work", Some("eu-west-2"));
    assert_eq!(s.profile.name, "work");
    assert_eq!(s.region, "eu-west-2");
    assert_eq!(s.store.bucket_region("anything"), None);
}

#[test]
fn connect_still_takes_a_profile_read_from_the_credentials_file() {
    let p = Profile {
        name: "legacy".into(),
        ..Profile::default()
    };
    let s = Session::connect(p, Some("ca-central-1"));
    assert_eq!(s.profile.name, "legacy");
    assert_eq!(s.region, "ca-central-1");
}

#[test]
fn a_bad_region_hint_is_not_used() {
    let s = Session::connect("work", Some("evil.example/#"));
    assert_ne!(s.region, "evil.example/#");
}
