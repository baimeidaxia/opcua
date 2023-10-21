// OPCUA for Rust
// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2017-2022 Adam Lock

use std::{
    fmt,
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};
use chrono::TimeZone;
use chrono_tz::Tz;

use chrono::TimeZone;
use chrono_tz::Tz;
use env_logger::{Builder, fmt::Color};

struct Pad<T> {
    value: T,
    width: usize,
}

impl<T: fmt::Display> fmt::Display for Pad<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{: <width$}", self.value, width = self.width)
    }
}

pub fn init() {
    init_inner(Tz::UTC);
}

pub fn init_with_tz(tz: Tz) {
    init_inner(tz);
}

pub fn init_inner(tz: Tz) {
    lazy_static! {
        static ref INITIALISED: AtomicBool = AtomicBool::new(false);
    }

    // Only need to call this once
    if !INITIALISED.swap(true, Ordering::Relaxed) {
        // This is env_logger::init() but taking logging values from  instead of RUST_LOG.
        // env_logger/RUST_LOG is used by cargo and other rust tools so console fills with garbage from
        // other processes  when we're only interested in our own garbage!
        let mut builder = Builder::from_env("RUST_OPCUA_LOG");
        builder.format(move |f, record| {
            let now = chrono::Utc::now();
            let now_tz = tz.from_utc_datetime(&now.naive_utc());
            let time_fmt = now_tz.format("%Y-%m-%d %H:%M:%S%.3f");

            let mut style = f.style();
            match record.metadata().level() {
                log::Level::Error => {
                    // White on red
                    style.set_color(Color::White);
                    style.set_bg(Color::Red);
                }
                log::Level::Warn => {
                    // Yellow on black
                    style.set_color(Color::Yellow);
                }
                log::Level::Info => {
                    // Blue on black
                    style.set_color(Color::Cyan);
                }
                log::Level::Debug => {
                    // Blue
                    style.set_color(Color::Green);
                }
                log::Level::Trace => {
                    // Grey
                    style.set_color(Color::Ansi256(8));
                }
            }
            let level = style.value(Pad {
                value: record.level(),
                width: 5,
            });

            let mut style = f.style();
            let target = style.set_bold(true).value(Pad {
                value: record.target(),
                width: 40,
            });

            let args = record.args();

            writeln!(f, "{} {} {} {}", time_fmt, level, target, args)
        });
        builder.init();
        info!("Logging is enabled, use RUST_OPCUA_LOG environment variable to control filtering, logging level");
    }
}