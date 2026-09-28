//! Numbers and dates in the user's locale, from the CLDR data ICU4X
//! compiles in: the decimal separator, digit grouping and digits for
//! numbers, and the order, separators and 12- or 24-hour clock for dates.
//!
//! Numbers reach the screen through Fluent: every `fl!` argument that is a
//! number is written by [`format_fluent_value`], which [`super`] installs
//! on each bundle. Code passes a plain integer, or a [`FluentNumber`] from
//! [`decimal`] to fix its fraction digits; words around a number (units, a
//! percent sign and its spacing) are the message's, so each language
//! places them. Dates are [`date_time`], not a Fluent value.
//!
//! The locale is the one the user asked for that speaks the chosen
//! translation's language (`en-GB` reads the en-US text with British
//! dates), else the translation's own ([`set_locale`], from
//! `i18n::select`).

use std::{
    cell::RefCell,
    sync::{PoisonError, RwLock},
};

use chrono::{Datelike, Timelike};
use fluent_bundle::{
    FluentValue,
    types::{FluentNumber, FluentNumberOptions},
};
use i18n_embed::unic_langid::LanguageIdentifier;
use icu_calendar::{Date, Gregorian};
use icu_datetime::{
    FixedCalendarDateTimeFormatter, fieldsets,
    input::{DateTime, Time},
};
use icu_decimal::{
    DecimalFormatter,
    input::{Decimal, FloatPrecision},
    options::{DecimalFormatterOptions, GroupingStrategy},
};
use icu_locale_core::Locale;

/// The locale numbers and dates are formatted in; en-US until
/// [`set_locale`].
static LOCALE: RwLock<Option<Locale>> = RwLock::new(None);

thread_local! {
    /// The formatters for [`LOCALE`], built on first use on each thread
    /// (ICU4X's formatters aren't `Sync`) and again after it changes.
    static FORMATTERS: RefCell<Option<Formatters>> = const { RefCell::new(None) };
}

struct Formatters {
    locale: Locale,
    grouped: DecimalFormatter,
    ungrouped: DecimalFormatter,
    date_time: FixedCalendarDateTimeFormatter<Gregorian, fieldsets::YMDT>,
}

impl Formatters {
    fn new(locale: Locale) -> Self {
        let decimal = |grouping: GroupingStrategy| {
            let options = DecimalFormatterOptions::from(grouping);
            DecimalFormatter::try_new((&locale).into(), options)
                .or_else(|_| DecimalFormatter::try_new(Default::default(), options))
                .expect("the root locale's number data is compiled in")
        };
        let date_time = FixedCalendarDateTimeFormatter::try_new(
            (&locale).into(),
            fieldsets::YMD::medium().with_time_hm(),
        )
        .or_else(|_| {
            FixedCalendarDateTimeFormatter::try_new(
                Default::default(),
                fieldsets::YMD::medium().with_time_hm(),
            )
        })
        .expect("the root locale's date data is compiled in");
        Self {
            grouped: decimal(GroupingStrategy::Auto),
            ungrouped: decimal(GroupingStrategy::Never),
            date_time,
            locale,
        }
    }
}

/// Format numbers and dates in `requested`'s first locale that speaks
/// `translation`'s language, else in `translation`.
pub(super) fn set_locale(requested: &[LanguageIdentifier], translation: &LanguageIdentifier) {
    let locale = to_locale(locale_for(requested, translation));
    tracing::info!(%locale, "formatting numbers and dates");
    *LOCALE.write().unwrap_or_else(PoisonError::into_inner) = Some(locale);
}

fn locale_for<'a>(
    requested: &'a [LanguageIdentifier],
    translation: &'a LanguageIdentifier,
) -> &'a LanguageIdentifier {
    requested
        .iter()
        .find(|language| language.language == translation.language)
        .unwrap_or(translation)
}

/// The ICU4X locale for a Fluent language tag; the root locale if ICU4X
/// can't read it.
fn to_locale(language: &LanguageIdentifier) -> Locale {
    Locale::try_from_str(&language.to_string()).unwrap_or(Locale::UNKNOWN)
}

#[cfg(test)]
thread_local! {
    /// In unit tests, this thread's locale instead of [`LOCALE`]: see
    /// `i18n::in_locale`.
    static THREAD_LOCALE: RefCell<Option<Locale>> = const { RefCell::new(None) };
}

/// Format on this thread only as [`set_locale`] would for `chosen`
/// (requested, translation), or as [`LOCALE`] says again for `None`.
#[cfg(test)]
pub(super) fn set_thread_locale(chosen: Option<(&[LanguageIdentifier], &LanguageIdentifier)>) {
    let locale =
        chosen.map(|(requested, translation)| to_locale(locale_for(requested, translation)));
    THREAD_LOCALE.set(locale);
}

/// This thread's locale in unit tests, if one was set.
#[cfg(test)]
fn thread_locale() -> Option<Locale> {
    THREAD_LOCALE.with_borrow(Clone::clone)
}

#[cfg(not(test))]
const fn thread_locale() -> Option<Locale> {
    None
}

fn with_formatters<T>(f: impl FnOnce(&Formatters) -> T) -> T {
    let locale = thread_locale().unwrap_or_else(|| {
        LOCALE
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or(icu_locale_core::locale!("en-US"))
    });
    FORMATTERS.with_borrow_mut(|formatters| {
        if formatters
            .as_ref()
            .is_none_or(|built| built.locale != locale)
        {
            *formatters = Some(Formatters::new(locale));
        }
        f(formatters.as_ref().expect("just built"))
    })
}

/// `value` as a Fluent argument shown with exactly `fraction_digits`
/// digits after the separator, rounding halves away from zero: `1.25`
/// with 1 reads "1.3" in en-US and "1,3" in German.
pub fn decimal(value: f64, fraction_digits: usize) -> FluentValue<'static> {
    FluentValue::Number(FluentNumber::new(
        value,
        FluentNumberOptions {
            minimum_fraction_digits: Some(fraction_digits),
            maximum_fraction_digits: Some(fraction_digits),
            ..FluentNumberOptions::default()
        },
    ))
}

/// Fluent's formatter hook (`FluentBundle::set_formatter`): numbers in
/// [`LOCALE`]; anything else as Fluent writes it.
pub(super) fn format_fluent_value<M>(value: &FluentValue, _: &M) -> Option<String> {
    match value {
        FluentValue::Number(number) => {
            Some(with_formatters(|formatters| formatters.number(number)))
        }
        _ => None,
    }
}

/// A local date and time, with minutes, in [`LOCALE`]'s medium format:
/// "Sep 24, 2026, 2:03 PM" in en-US, "24.09.2026, 14:03" in German.
pub fn date_time(time: &chrono::DateTime<chrono::Local>) -> String {
    with_formatters(|formatters| formatters.date_time(time))
}

impl Formatters {
    fn number(&self, number: &FluentNumber) -> String {
        let options = &number.options;
        let mut decimal = to_decimal(number.value, options.maximum_fraction_digits);
        if let Some(minimum) = options.minimum_fraction_digits {
            decimal.pad_end(-clamp_digits(minimum));
        }
        let formatter = if options.use_grouping {
            &self.grouped
        } else {
            &self.ungrouped
        };
        without_narrow_spaces(formatter.format_to_string(&decimal))
    }

    fn date_time(&self, time: &chrono::DateTime<chrono::Local>) -> String {
        let Some(date) = u8::try_from(time.month())
            .ok()
            .zip(u8::try_from(time.day()).ok())
            .and_then(|(month, day)| Date::try_new_gregorian(time.year(), month, day).ok())
        else {
            return String::new();
        };
        let Ok(clock) = Time::try_new(
            u8::try_from(time.hour()).unwrap_or_default(),
            u8::try_from(time.minute()).unwrap_or_default(),
            0,
            0,
        ) else {
            return String::new();
        };
        let text = self
            .date_time
            .format(&DateTime { date, time: clock })
            .to_string();
        without_narrow_spaces(text)
    }
}

/// `value` as a decimal, rounded to at most `maximum` fraction digits.
fn to_decimal(value: f64, maximum: Option<usize>) -> Decimal {
    if let Some(maximum) = maximum {
        let digits = clamp_digits(maximum);
        // `f64::round` rounds halves away from zero, as people do; the
        // `as` saturates, which only a number far beyond any size reaches.
        #[allow(clippy::cast_possible_truncation)]
        let scaled = (value * 10_f64.powi(i32::from(digits))).round() as i128;
        let mut decimal = Decimal::from(scaled);
        decimal.multiply_pow10(-digits);
        decimal.trim_end();
        return decimal;
    }
    Decimal::try_from_f64(value, FloatPrecision::RoundTrip).unwrap_or_default()
}

fn clamp_digits(digits: usize) -> i16 {
    i16::try_from(digits.min(20)).expect("at most 20")
}

/// CLDR puts a narrow no-break space (U+202F) before "PM" and between
/// French digit groups. The bundled Instrument Sans font has no glyph for it, so
/// it becomes a no-break space, which it has and which also doesn't wrap.
fn without_narrow_spaces(text: String) -> String {
    if text.contains('\u{202F}') {
        text.replace('\u{202F}', "\u{A0}")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn formatters(language: &str) -> Formatters {
        Formatters::new(Locale::try_from_str(language).unwrap())
    }

    fn number(language: &str, value: f64, digits: Option<usize>) -> String {
        let number = match digits {
            Some(digits) => match decimal(value, digits) {
                FluentValue::Number(number) => number,
                _ => unreachable!(),
            },
            None => FluentNumber::new(value, FluentNumberOptions::default()),
        };
        formatters(language).number(&number)
    }

    #[test]
    fn numbers_follow_the_locale() {
        assert_eq!(number("en-US", 1.25, Some(1)), "1.3");
        assert_eq!(number("en-US", 1.0, Some(1)), "1.0");
        assert_eq!(number("en-US", 9.96, Some(1)), "10.0");
        assert_eq!(number("en-US", 1023.0, None), "1,023");
        assert_eq!(number("en-US", 82.0, None), "82");
        assert_eq!(number("en-US", 0.5, None), "0.5");
        assert_eq!(number("de", 1.25, Some(1)), "1,3");
        assert_eq!(number("de", 12345.0, None), "12.345");
        assert_eq!(number("fr", 12345.0, None), "12\u{A0}345");
        assert_eq!(number("zh-CN", 1.5, Some(1)), "1.5");
        // The pseudo-locale formats as English.
        assert_eq!(number("en-XA", 12345.5, None), "12,345.5");
    }

    #[test]
    fn tests_format_in_en_us() {
        let one = FluentNumber::new(1234.5, FluentNumberOptions::default());
        assert_eq!(
            format_fluent_value(&FluentValue::Number(one), &()).as_deref(),
            Some("1,234.5")
        );
        assert_eq!(format_fluent_value(&FluentValue::from("x"), &()), None);
    }

    #[test]
    fn a_locale_that_speaks_the_translation_formats() {
        let tags = |tags: &[&str]| -> Vec<LanguageIdentifier> {
            tags.iter().map(|tag| tag.parse().unwrap()).collect()
        };
        let en_us = &tags(&["en-US"])[0];
        assert_eq!(locale_for(&tags(&["en-GB"]), en_us).to_string(), "en-GB");
        let requested = tags(&["fr-FR", "en-AU"]);
        assert_eq!(locale_for(&requested, en_us).to_string(), "en-AU");
        assert_eq!(locale_for(&tags(&["fr-FR"]), en_us).to_string(), "en-US");
        assert_eq!(to_locale(&tags(&["zh-CN"])[0]).to_string(), "zh-CN");
    }

    #[test]
    fn dates_follow_the_locale() {
        let at = chrono::Local
            .with_ymd_and_hms(2026, 9, 24, 14, 3, 59)
            .unwrap();
        assert_eq!(date_time(&at), "Sep 24, 2026, 2:03\u{A0}PM");
        assert_eq!(formatters("de").date_time(&at), "24.09.2026, 14:03");
        assert_eq!(formatters("en-GB").date_time(&at), "24 Sept 2026, 14:03");
        assert_eq!(formatters("zh-CN").date_time(&at), "2026年9月24日 14:03");
    }
}
