//! The mascot's mood: what a session is up to, as one of its sprites ([`mascot`]).

use chrono::{DateTime, Days, Local, NaiveTime, TimeZone, Timelike};

use crate::mascot::{self, Sprite};

/// How long a session that finished its turn looks pleased with itself, in milliseconds.
pub const DONE_FOR: u64 = 5 * 60 * 1000;
/// A session that went idle this soon after it started never did anything yet, in milliseconds.
const FRESH: u64 = 10_000;
/// Night, in local hours: from the first to the second.
const NIGHT: (u32, u32) = (22, 7);

/// Least to most pressing: the bar shows the most pressing of its sessions'.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mood {
    /// Idle a while, at night.
    Sleeping,
    /// Idle a while, by day.
    Coffee,
    /// Its turn ended a moment ago.
    Happy,
    /// Working.
    Laptop,
    /// Working in plan mode.
    Planning,
    /// Waiting for you: a permission, a question, a plan to approve.
    Question,
}

impl Mood {
    pub const ALL: [Mood; 6] = [
        Mood::Sleeping,
        Mood::Coffee,
        Mood::Happy,
        Mood::Laptop,
        Mood::Planning,
        Mood::Question,
    ];

    pub fn sprite(self) -> &'static Sprite {
        match self {
            Mood::Sleeping => &mascot::SLEEPING,
            Mood::Coffee => &mascot::COFFEE,
            Mood::Happy => &mascot::HAPPY,
            Mood::Laptop => &mascot::LAPTOP,
            Mood::Planning => &mascot::PLANNING,
            Mood::Question => &mascot::QUESTION,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mood::Sleeping => "sleeping",
            Mood::Coffee => "coffee",
            Mood::Happy => "happy",
            Mood::Laptop => "laptop",
            Mood::Planning => "planning",
            Mood::Question => "question",
        }
    }
}

/// What the plugin knows about a session, for its mood.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    /// A prompt waits for an answer, or the terminal shows a dialog.
    pub asked: bool,
    pub busy: bool,
    /// In plan mode, as its hooks last said.
    pub plan: bool,
    /// Its last turn ended in an error.
    pub failed: bool,
    /// When it started, and when its status last changed: milliseconds since the epoch.
    pub started: u64,
    pub since: u64,
}

/// The time, as moods need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Clock {
    /// Milliseconds since the epoch.
    pub now: u64,
    pub night: bool,
}

impl Clock {
    pub fn now() -> Self {
        let now = Local::now();
        Self {
            now: u64::try_from(now.timestamp_millis()).unwrap_or(0),
            night: night(now.hour()),
        }
    }
}

pub fn mood(facts: &Facts, clock: Clock) -> Mood {
    if facts.asked {
        Mood::Question
    } else if facts.busy && facts.plan {
        Mood::Planning
    } else if facts.busy {
        Mood::Laptop
    } else if happy(facts, clock.now) {
        Mood::Happy
    } else if clock.night {
        Mood::Sleeping
    } else {
        Mood::Coffee
    }
}

fn happy(facts: &Facts, now: u64) -> bool {
    !facts.failed
        && facts.since >= facts.started + FRESH
        && now.saturating_sub(facts.since) < DONE_FOR
}

/// When time alone next changes this session's mood, if it can: milliseconds since the epoch.
/// `boundary` is when night next starts or ends.
pub fn next_change(facts: &Facts, clock: Clock, boundary: u64) -> Option<u64> {
    match mood(facts, clock) {
        Mood::Happy => Some(facts.since + DONE_FOR),
        Mood::Coffee | Mood::Sleeping => Some(boundary),
        _ => None,
    }
}

fn night(hour: u32) -> bool {
    hour >= NIGHT.0 || hour < NIGHT.1
}

/// When night next starts or ends, after `now`: milliseconds since the epoch.
pub fn next_boundary() -> u64 {
    boundary_after(Local::now())
}

fn boundary_after<Tz: TimeZone>(now: DateTime<Tz>) -> u64 {
    let today = now.date_naive();
    let days = [Some(today), today.checked_add_days(Days::new(1))];
    days.into_iter()
        .flatten()
        .flat_map(|day| [NIGHT.1, NIGHT.0].map(|hour| day.and_time(at(hour))))
        .filter_map(|time| now.timezone().from_local_datetime(&time).earliest())
        .find(|time| *time > now)
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .unwrap_or(u64::MAX)
}

fn at(hour: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(hour, 0, 0).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    const MINUTE: u64 = 60 * 1000;

    fn idle(since: u64) -> Facts {
        Facts {
            started: 0,
            since,
            ..Facts::default()
        }
    }

    fn day(now: u64) -> Clock {
        Clock { now, night: false }
    }

    #[test]
    fn asking_wins_then_working_then_resting() {
        let busy = Facts {
            busy: true,
            ..idle(MINUTE)
        };
        let clock = day(2 * MINUTE);
        assert_eq!(mood(&busy, clock), Mood::Laptop);
        let planning = Facts { plan: true, ..busy };
        assert_eq!(mood(&planning, clock), Mood::Planning);
        let asked = Facts {
            asked: true,
            ..planning
        };
        assert_eq!(mood(&asked, clock), Mood::Question);
        assert_eq!(mood(&idle(MINUTE), clock), Mood::Happy);
    }

    #[test]
    fn done_is_happy_for_five_minutes_then_coffee_or_sleep() {
        let facts = idle(MINUTE);
        assert_eq!(mood(&facts, day(MINUTE + DONE_FOR - 1)), Mood::Happy);
        assert_eq!(mood(&facts, day(MINUTE + DONE_FOR)), Mood::Coffee);
        let night = Clock {
            now: MINUTE + DONE_FOR,
            night: true,
        };
        assert_eq!(mood(&facts, night), Mood::Sleeping);
        assert_eq!(
            next_change(&facts, day(MINUTE), 99 * MINUTE),
            Some(MINUTE + DONE_FOR)
        );
        assert_eq!(
            next_change(&facts, day(MINUTE + DONE_FOR), 99 * MINUTE),
            Some(99 * MINUTE)
        );
    }

    #[test]
    fn a_fresh_or_failed_session_isnt_happy() {
        assert_eq!(mood(&idle(FRESH - 1), day(FRESH)), Mood::Coffee);
        let failed = Facts {
            failed: true,
            ..idle(MINUTE)
        };
        assert_eq!(mood(&failed, day(2 * MINUTE)), Mood::Coffee);
    }

    #[test]
    fn night_runs_from_ten_to_seven() {
        assert!(night(22) && night(23) && night(0) && night(6));
        assert!(!night(7) && !night(12) && !night(21));
    }

    #[test]
    fn the_next_boundary_is_seven_or_ten_whichever_comes_first() {
        let at = |h, m| Utc.with_ymd_and_hms(2026, 10, 13, h, m, 0).unwrap();
        let ms = |time: DateTime<Utc>| time.timestamp_millis() as u64;
        assert_eq!(boundary_after(at(12, 0)), ms(at(22, 0)));
        assert_eq!(boundary_after(at(3, 0)), ms(at(7, 0)));
        assert_eq!(boundary_after(at(7, 0)), ms(at(22, 0)));
        assert_eq!(
            boundary_after(at(23, 30)),
            ms(at(7, 0) + chrono::Duration::days(1))
        );
    }
}
