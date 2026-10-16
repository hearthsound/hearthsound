//! Tests for the import path template: rendering, optional groups, padding,
//! sanitization, and rejection of templates that would escape the root.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use hearthsound_config::{SettingsRegistry, SettingsStore};
use hearthsound_library::naming::{
    NamingContext, NamingError, NamingTemplate, NamingTemplateError, sanitize_component,
};

const DEFAULT_TEMPLATE: &str =
    "{album_artist}/{album}[ ({year})]/{disc_prefix}{track:02} - {title}.{ext}";

/// Renders `template` with `context`.
fn render(template: &str, context: &NamingContext<'_>) -> Result<PathBuf, NamingError> {
    NamingTemplate::parse(template)?.render(context)
}

/// The context the table below renders with, adjusted per case.
fn context<'a>() -> NamingContext<'a> {
    NamingContext {
        album_artist: Some("Energy 52"),
        album: Some("Cafe del Mar"),
        year: Some(1993),
        disc_number: Some(1),
        disc_total: Some(1),
        track_number: Some(1),
        title: Some("Cafe del Mar"),
        extension: Some("flac"),
    }
}

#[test]
fn the_default_template_renders_the_documented_path() {
    assert_eq!(
        render(DEFAULT_TEMPLATE, &context()).unwrap(),
        Path::new("Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );
}

#[test]
fn the_no_year_example_drops_the_optional_group() {
    let context = NamingContext {
        year: None,
        ..context()
    };
    assert_eq!(
        render(DEFAULT_TEMPLATE, &context).unwrap(),
        Path::new("Energy 52/Cafe del Mar/01 - Cafe del Mar.flac"),
        "an album with no year must not render `Album ()`"
    );
}

#[test]
fn the_acceptance_template_renders_its_shape() {
    // The acceptance shape `<Album Artist>/<Album> (<Year>)/NN - <Title>.<ext>`
    // spelled with tokens.
    let template = "{album_artist}/{album} ({year})/{track:02} - {title}.{ext}";
    assert_eq!(
        render(template, &context()).unwrap(),
        Path::new("Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );

    // Without a year the parentheses stay, because they are literal text in
    // this shape; the default template's optional group is what drops them.
    let context = NamingContext {
        year: None,
        ..context()
    };
    assert_eq!(
        render(template, &context).unwrap(),
        Path::new("Energy 52/Cafe del Mar ()/01 - Cafe del Mar.flac")
    );
}

#[test]
fn the_disc_prefix_appears_only_on_multi_disc_releases() {
    let single = context();
    assert_eq!(
        render(DEFAULT_TEMPLATE, &single).unwrap(),
        Path::new("Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );

    let multi = NamingContext {
        disc_number: Some(2),
        disc_total: Some(2),
        ..context()
    };
    assert_eq!(
        render(DEFAULT_TEMPLATE, &multi).unwrap(),
        Path::new("Energy 52/Cafe del Mar (1993)/2-01 - Cafe del Mar.flac")
    );

    // A second disc with no stated total is still multi-disc.
    let implied = NamingContext {
        disc_number: Some(2),
        disc_total: None,
        ..context()
    };
    assert_eq!(
        render(DEFAULT_TEMPLATE, &implied).unwrap(),
        Path::new("Energy 52/Cafe del Mar (1993)/2-01 - Cafe del Mar.flac")
    );

    // A multi-disc release with no disc number renders no prefix.
    let missing = NamingContext {
        disc_number: None,
        disc_total: Some(2),
        ..context()
    };
    assert_eq!(
        render(DEFAULT_TEMPLATE, &missing).unwrap(),
        Path::new("Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );
}

#[test]
fn padding_widths_are_honoured() {
    let table = [
        ("{track}", "7"),
        ("{track:01}", "7"),
        ("{track:02}", "07"),
        ("{track:03}", "007"),
        ("{track:02}", "123"),
    ];
    let numbers = [7, 7, 7, 7, 123];
    for ((template, expected), number) in table.into_iter().zip(numbers) {
        let context = NamingContext {
            track_number: Some(number),
            ..context()
        };
        assert_eq!(
            render(template, &context).unwrap(),
            Path::new(expected),
            "{template} with {number}"
        );
    }

    // A missing number renders empty and drops the component.
    let context = NamingContext {
        track_number: None,
        ..context()
    };
    assert_eq!(render("{track:02}", &context), Err(NamingError::EmptyPath));
}

#[test]
fn optional_groups_drop_when_any_inner_token_is_empty() {
    let table = [
        ("a[{album}][ ({year})]z", Some("aCafe del Mar (1993)z")),
        ("a[{album}][ ({year})]z", Some("aCafe del Marz")),
        ("a[{album}][ ({year})]z", Some("az")),
        ("[({year})]", Some("(1993)")),
        ("[({year})]", None),
        ("[literal]", Some("literal")),
        ("[ {title} - {year} ]", Some(" Cafe del Mar - 1993")),
    ];
    let contexts = [
        context(),
        NamingContext {
            year: None,
            ..context()
        },
        NamingContext {
            album: None,
            year: None,
            ..context()
        },
        context(),
        NamingContext {
            year: None,
            ..context()
        },
        NamingContext::default(),
        context(),
    ];
    for ((template, expected), context) in table.into_iter().zip(contexts) {
        match expected {
            Some(expected) => assert_eq!(
                render(template, &context).unwrap(),
                Path::new(expected),
                "{template}"
            ),
            None => assert_eq!(
                render(template, &context),
                Err(NamingError::EmptyPath),
                "{template} renders nothing"
            ),
        }
    }
}

#[test]
fn empty_components_are_dropped() {
    let context = NamingContext {
        album_artist: None,
        ..context()
    };
    assert_eq!(
        render(DEFAULT_TEMPLATE, &context).unwrap(),
        Path::new("Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );

    // A template that renders nothing at all is rejected.
    let context = NamingContext::default();
    assert_eq!(render("[{year}]", &context), Err(NamingError::EmptyPath));
    assert_eq!(
        render("//", &NamingContext::default()),
        Err(NamingError::EmptyPath)
    );
}

#[test]
fn components_are_sanitized_for_the_filesystem() {
    let table = [
        ("plain", "plain"),
        ("a/b", "a_b"),
        ("a\\b", "a_b"),
        ("a:b*c?d\"e<f>g|h", "a_b_c_d_e_f_g_h"),
        ("tab\there", "tab_here"),
        ("dot.", "dot"),
        ("spaces   ", "spaces"),
        ("dot. ", "dot"),
        // Sanitizing alone trims the dots; `render` rejects a `..` component
        // before sanitizing so it can never be silently dropped.
        ("..", ""),
        ("...", ""),
        ("CON", "_CON"),
        ("nul.txt", "_nul.txt"),
        ("lpt9", "_lpt9"),
        ("Lpt1", "_Lpt1"),
        ("com0", "com0"),
        ("CONSOLE", "CONSOLE"),
    ];
    for (input, expected) in table {
        assert_eq!(sanitize_component(input), expected, "{input}");
    }
}

#[test]
fn components_are_capped_at_two_hundred_bytes() {
    let long = "a".repeat(300);
    assert_eq!(sanitize_component(&long).len(), 200);

    // The cap never splits a UTF-8 sequence.
    let multibyte = "a".repeat(199) + "\u{e9}";
    let capped = sanitize_component(&multibyte);
    assert_eq!(capped.len(), 199);
    assert!(capped.chars().all(|character| character == 'a'));

    let exact = "\u{e9}".repeat(100);
    assert_eq!(sanitize_component(&exact).len(), 200);

    // A trailing dot revealed by the cut is trimmed as well.
    let dotted = "a".repeat(199) + ".";
    assert_eq!(sanitize_component(&dotted).len(), 199);
}

#[test]
fn templates_that_escape_the_root_are_rejected() {
    assert_eq!(
        render("../{title}", &context()),
        Err(NamingError::EscapesRoot)
    );
    assert_eq!(
        render("{album_artist}/../{title}", &context()),
        Err(NamingError::EscapesRoot)
    );
    // A token value of `..` is rejected too.
    let context = NamingContext {
        title: Some(".."),
        ..context()
    };
    assert_eq!(render("{title}", &context), Err(NamingError::EscapesRoot));
}

#[test]
fn malformed_templates_are_rejected() {
    assert_eq!(
        NamingTemplate::parse("{nope}"),
        Err(NamingError::UnknownToken {
            name: "nope".to_owned()
        })
    );
    assert_eq!(
        NamingTemplate::parse("{title"),
        Err(NamingError::UnterminatedToken("{title".to_owned()))
    );
    assert_eq!(
        NamingTemplate::parse("[{title}"),
        Err(NamingError::UnterminatedGroup)
    );
    assert_eq!(
        NamingTemplate::parse("{title}]"),
        Err(NamingError::UnexpectedGroupClose)
    );
    assert_eq!(
        NamingTemplate::parse("{track:0}"),
        Err(NamingError::InvalidPadding {
            value: "0".to_owned()
        })
    );
    assert_eq!(
        NamingTemplate::parse("{track:x}"),
        Err(NamingError::InvalidPadding {
            value: "x".to_owned()
        })
    );
    assert_eq!(
        NamingTemplate::parse("{track:10}"),
        Err(NamingError::InvalidPadding {
            value: "10".to_owned()
        })
    );
    assert_eq!(
        NamingTemplate::parse("{title:02}"),
        Err(NamingError::PaddingNotAllowed {
            token: "title".to_owned()
        })
    );
}

#[test]
fn render_under_joins_the_destination_root() {
    let template = NamingTemplate::parse(DEFAULT_TEMPLATE).unwrap();
    assert_eq!(
        template
            .render_under(Path::new("/srv/library"), &context())
            .unwrap(),
        Path::new("/srv/library/Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );
}

#[test]
fn the_template_is_read_from_the_settings_registry() {
    let settings = SettingsStore::defaults(&SettingsRegistry::builtin());
    let template = NamingTemplate::from_settings(&settings).unwrap();
    assert_eq!(
        template.render(&context()).unwrap(),
        Path::new("Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );

    // A template error surfaces as the wrapping error, so a caller can match it.
    let error = NamingTemplateError::from(NamingError::EmptyPath);
    assert!(matches!(
        error,
        NamingTemplateError::Template(NamingError::EmptyPath)
    ));
}
