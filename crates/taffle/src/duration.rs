//! Durations as somebody types them and as they are shown back: `4.4`, `12:34`, `1:02:10.5`.
//!
//! # What a component may hold
//!
//! A duration is up to three components separated by colons, with thousandths behind the last of
//! them. Every component but the first is a subdivision of the one in front of it and is bounded by
//! it: `1:99` is refused rather than read as 2:39, because 99 seconds in the seconds of a clock is
//! something other than what was meant, and a converter that guesses which is worse than one that
//! says so.
//!
//! The component a duration *begins* with has nothing in front of it to run over into, and is
//! bounded by nothing: `90:00` is an hour and a half and `3600` is an hour. Which is what lets a
//! chapter list stay in one unit throughout — minutes past the hour, seconds past the minute —
//! instead of changing shape halfway down.
//!
//! # Digits, and nothing else
//!
//! What a component holds is ASCII digits. Everything else a number can be typed as — a sign, an
//! exponent, a hexadecimal prefix, the words a float parses as — is no time of day and no length of
//! audio, so `-3`, `1e3` and `inf` are refused along with `abc`.

use std::str::FromStr;
use std::time::Duration;

/// The frames of one channel a second of a TAF's audio comes to.
pub const RATE: u32 = 48_000;

/// What a component behind the first one counts up to: sixty of it are one of the component in
/// front of it, whichever two of hours, minutes and seconds those are.
const PER_COMPONENT: u64 = 60;

/// The components a duration is typed in at the most: hours, minutes, seconds.
const COMPONENTS: usize = 3;

/// A length of audio in seconds, as it was typed on the command line.
///
/// [`FromStr`] is the only way to one, so what is inside is a duration somebody typed: not
/// negative, not a NaN, and no longer than the seconds a `u64` counts.
#[derive(Debug, Clone, Copy)]
pub struct Seconds(f64);

impl Seconds {
    /// The frames of one channel this comes to at the 48 kHz a TAF is counted in, at the nearest
    /// frame to the time that was typed.
    pub fn to_samples_48k(self) -> u64 {
        // The rounding is what makes 4.4 seconds the 211 200th frame rather than the one before it:
        // a tenth of a second is no exact binary fraction, so the product lands a hair short of the
        // frame it means. What was parsed is finite and not negative, and the cast saturates at
        // both ends of the range regardless.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frames = (self.0 * f64::from(RATE)).round() as u64;

        frames
    }
}

impl FromStr for Seconds {
    type Err = InvalidDuration;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        seconds(text)
            .map(Self)
            .ok_or_else(|| InvalidDuration(text.to_owned()))
    }
}

/// A duration that is no duration.
#[derive(Debug, thiserror::Error)]
#[error("invalid duration '{0}'")]
pub struct InvalidDuration(pub String);

/// How long `duration` is, on a clock: `0:10`, `12:34`, `1:02:10` — the hours only where there are
/// any, and the second being played not counted until it has been.
pub fn clock(duration: Duration) -> String {
    let played = duration.as_secs();
    let hours = played / (PER_COMPONENT * PER_COMPONENT);
    let minutes = (played / PER_COMPONENT) % PER_COMPONENT;
    let seconds = played % PER_COMPONENT;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The seconds `text` states, or nothing at all where it states no duration.
fn seconds(text: &str) -> Option<f64> {
    let components: Vec<&str> = text.split(':').collect();
    if components.len() > COMPONENTS {
        return None;
    }

    let mut whole: u64 = 0;
    let mut thousandths = 0.0;

    for (at, component) in components.iter().enumerate() {
        // The seconds are the only component a fraction can hang off, and they are the last one.
        let value = if at + 1 == components.len() {
            let (seconds, fraction) = fractional(component)?;
            thousandths = fraction;
            seconds
        } else {
            digits(component)?
        };

        // Every component behind the first is bounded by the one in front of it, which the first
        // one has none of.
        if at > 0 && value >= PER_COMPONENT {
            return None;
        }
        whole = whole.checked_mul(PER_COMPONENT)?.checked_add(value)?;
    }

    // A duration a person types is far inside the integers a float holds exactly: a `u64` of
    // seconds runs out at 585 billion years.
    #[allow(clippy::cast_precision_loss)]
    let total = whole as f64 + thousandths;

    Some(total)
}

/// The whole seconds `component` states and the fraction of one behind its point.
fn fractional(component: &str) -> Option<(u64, f64)> {
    let Some((whole, thousandths)) = component.split_once('.') else {
        return Some((digits(component)?, 0.0));
    };
    if !only_digits(thousandths) {
        return None;
    }

    // What the digits behind the point come to, read the way a float parser reads a fraction:
    // rounded to the nearest float there is, which is nearer than a count of them divided by a
    // power of ten. Digits behind a point are a number in every case, and what is somehow not one
    // is no part of a second.
    let fraction = format!("0.{thousandths}").parse().unwrap_or(0.0);

    Some((digits(whole)?, fraction))
}

/// The number `text` states, where it is digits and there are not more of them than a count of
/// seconds holds.
fn digits(text: &str) -> Option<u64> {
    if !only_digits(text) {
        return None;
    }

    text.parse().ok()
}

/// Whether `text` is ASCII digits and nothing else, which is what a component of a duration is
/// written in.
fn only_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::{clock, Seconds};
    use std::time::Duration;

    /// The samples `text` comes to at 48 kHz, where it is a duration at all.
    fn samples(text: &str) -> u64 {
        let seconds: Seconds = text.parse().expect("a duration");

        seconds.to_samples_48k()
    }

    #[test]
    fn seconds_are_seconds_and_the_thousandths_behind_them() {
        assert_eq!(samples("0"), 0);
        assert_eq!(samples("4.4"), 211_200);
        assert_eq!(samples("0.5"), 24_000);
        // A whole hour typed as seconds is an hour: what a single component states is not bounded
        // by anything, since there is no component in front of it for it to have run over into.
        assert_eq!(samples("3600"), 172_800_000);
    }

    #[test]
    fn minutes_and_seconds_are_the_clock_they_look_like() {
        assert_eq!(samples("12:34"), 754 * 48_000);
        assert_eq!(samples("0:00"), 0);
        assert_eq!(samples("1:02:10.5"), 179_064_000);
        assert_eq!(samples("0:00:01"), 48_000);
    }

    #[test]
    fn the_component_a_duration_begins_with_counts_as_far_as_it_likes() {
        // 90 minutes is an hour and a half, and nothing in front of the minutes says otherwise: a
        // component is bounded by the one to its left, and the leftmost has none. Which is what
        // lets a chapter list be typed in minutes throughout rather than in hours from 60 on.
        assert_eq!(samples("90:00"), 5_400 * 48_000);
        assert_eq!(samples("100:00:00"), 360_000 * 48_000);
    }

    #[test]
    fn a_component_that_ran_over_into_the_one_in_front_of_it_is_no_duration() {
        // 99 seconds is a minute and 39 seconds, and somebody who typed it into the seconds of a
        // clock meant something else — so it is refused rather than read as either.
        for text in ["1:99", "1:60", "0:00:60", "0:60:00"] {
            assert!(text.parse::<Seconds>().is_err(), "{text} parsed");
        }
    }

    #[test]
    fn what_is_no_clock_and_no_number_is_no_duration() {
        let refused = [
            // One component more than a clock has, and none at all.
            "1:2:3:4",
            "",
            "abc",
            // A component of nothing, which is what a stray colon leaves.
            ":30",
            "1:",
            "1::2",
            // A point with no thousandths behind it, or in a component that has no thousandths.
            "4.",
            ".4",
            "4.4.4",
            "1:2.5:3",
            // Every way a number can be typed that is not the digits of one.
            "-3",
            "+3",
            " 4",
            "4 ",
            "0x10",
            "1e3",
            "inf",
            "NaN",
            "1_000",
            // More seconds than a count of them holds, whichever component states them.
            "99999999999999999999",
            "999999999999999999:00",
            "307445734561825860:16",
        ];

        for text in refused {
            assert!(text.parse::<Seconds>().is_err(), "{text} parsed");
        }
    }

    #[test]
    fn a_duration_that_is_no_duration_says_what_was_typed() {
        let error = "1:99".parse::<Seconds>().expect_err("no duration");

        assert_eq!(error.to_string(), "invalid duration '1:99'");
    }

    #[test]
    fn a_frame_is_the_nearest_one_to_the_time_that_was_typed() {
        // 4.4 seconds is 211 199.999… frames the way a float holds it, and the frame it means is
        // the one it is a hair short of rather than the one before that.
        assert_eq!(samples("4.4"), 211_200);
        // A time inside a frame is the frame it is nearest to, which is where a duration finer
        // than the audio it is about ends up.
        assert_eq!(samples("0.00001"), 0);
        assert_eq!(samples("0.00002"), 1);
        // Thousandths finer than a float holds are the float they round to, which is a duration
        // like any other and not something to refuse.
        assert_eq!(samples("0.99999999999999999999"), 48_000);
    }

    #[test]
    fn a_clock_shows_the_hours_a_length_has_and_no_more() {
        assert_eq!(clock(Duration::ZERO), "0:00");
        assert_eq!(clock(Duration::from_secs(10)), "0:10");
        assert_eq!(clock(Duration::from_secs(754)), "12:34");
        assert_eq!(clock(Duration::from_secs(3_730)), "1:02:10");
        assert_eq!(clock(Duration::from_secs(360_000)), "100:00:00");
        // What is not a whole second yet has not been played yet.
        assert_eq!(clock(Duration::from_millis(1_999)), "0:01");
    }
}
