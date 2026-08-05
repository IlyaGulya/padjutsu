use serde::Deserialize;

use crate::{v1::ProfileV1, Profile, profile::ProfileError};

/// Parse yaml profile.
pub fn parse_profile(input: &str) -> Result<Profile, ProfileError> {
    let version = parse_version(input)?;
    match version {
        1 => {
            let profile: ProfileV1 = serde_yaml::from_str(input)?;
            let workspace = profile.parse()?;
            Ok(workspace)
        }
        _ => Err(ProfileError::UnsupportedVersion(version)),
    }
}

/// A profile with a version.
#[derive(Debug, Clone, Deserialize)]
struct VersionedProfile {
    version: u8,
}

/// Parse the version of yaml profile.
fn parse_version(input: &str) -> Result<u8, ProfileError> {
    let raw: VersionedProfile = serde_yaml::from_str(input)?;
    Ok(raw.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StickMode;

    #[test]
    fn parse_profile_yaml_error_when_version_missing() {
        let yaml = "controllers: []\n";
        assert!(matches!(
            parse_profile(yaml),
            Err(ProfileError::YamlDeserializeError(_))
        ));
    }

    #[test]
    fn parse_scroll_axis_lock_enabled() {
        let yaml = r#"
version: 1
rules:
  common:
    sticks:
      right:
        mode: scroll
        horizontal: true
        axis_lock: true
"#;
        let profile = parse_profile(yaml).expect("should parse");
        let rules = profile.rules.get("common").expect("common rules");
        let right = rules
            .sticks
            .get(&crate::StickSide::Right)
            .expect("right stick");
        match right {
            StickMode::Scroll(params) => {
                assert!(params.horizontal);
                assert!(params.axis_lock);
            }
            other => panic!("expected Scroll, got {other:?}"),
        }
    }

    #[test]
    fn parse_scroll_axis_lock_defaults_to_false() {
        let yaml = r#"
version: 1
rules:
  common:
    sticks:
      right:
        mode: scroll
        horizontal: true
"#;
        let profile = parse_profile(yaml).expect("should parse");
        let rules = profile.rules.get("common").expect("common rules");
        let right = rules
            .sticks
            .get(&crate::StickSide::Right)
            .expect("right stick");
        match right {
            StickMode::Scroll(params) => {
                assert!(params.horizontal);
                assert!(!params.axis_lock, "axis_lock should default to false");
            }
            other => panic!("expected Scroll, got {other:?}"),
        }
    }

    #[test]
    fn parse_trackpad_scroll_defaults_to_free_two_axis_motion() {
        let yaml = r#"
version: 1
rules:
  common:
    sticks:
      right:
        mode: trackpad_scroll
"#;
        let profile = parse_profile(yaml).expect("should parse");
        let rules = profile.rules.get("common").expect("common rules");
        let right = rules
            .sticks
            .get(&crate::StickSide::Right)
            .expect("right stick");
        match right {
            StickMode::TrackpadScroll(params) => {
                assert!(params.horizontal);
                assert!(!params.axis_lock);
            }
            other => panic!("expected TrackpadScroll, got {other:?}"),
        }
    }
}
