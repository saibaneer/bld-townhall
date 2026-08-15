#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(BookingId);
id_type!(VenueId);
id_type!(SlotId);
id_type!(PrincipalId);
id_type!(ActorId);
id_type!(EffectIntentId);
id_type!(CouncilBookingRef);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Money {
    pence: u64,
}

impl Money {
    #[must_use]
    pub const fn from_pence(pence: u64) -> Self {
        Self { pence }
    }

    #[must_use]
    pub const fn pence(self) -> u64 {
        self.pence
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeWindow {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingRequirements {
    pub purpose: String,
    pub requested_date: String,
    pub time_window: TimeWindow,
    pub attendees: u16,
    pub wheelchair_accessible: bool,
    pub max_fee: Money,
}
