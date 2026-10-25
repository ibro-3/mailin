//! WebKit view for message bodies.
//!
//! The document, Content-Security-Policy, `cid:` rewriting and remote-content
//! detection live in [`mailin_core::html`] so the GTK and WinUI clients render
//! mail identically. This module only adds the WebKit-specific view and its
//! navigation policy.

use gtk::prelude::*;
use webkit6::prelude::*;

pub use mailin_core::html::{data_url, document, has_remote_content};

/// Create a WebView locked down for email rendering.
///
/// Links are handed to the desktop instead of navigating the reader.
pub fn new_view() -> webkit6::WebView {
    let settings = webkit6::Settings::new();
    settings.set_enable_javascript(false);
    settings.set_enable_javascript_markup(false);
    settings.set_enable_webgl(false);
    settings.set_enable_media(false);
    settings.set_enable_media_stream(false);
    settings.set_enable_mediasource(false);
    settings.set_enable_html5_database(false);
    settings.set_enable_html5_local_storage(false);
    settings.set_enable_developer_extras(false);
    settings.set_allow_file_access_from_file_urls(false);
    settings.set_allow_universal_access_from_file_urls(false);

    let view = webkit6::WebView::builder()
        .settings(&settings)
        .hexpand(true)
        .vexpand(true)
        .build();
    view.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));

    view.connect_decide_policy(|_, decision, kind| {
        if kind != webkit6::PolicyDecisionType::NavigationAction
            && kind != webkit6::PolicyDecisionType::NewWindowAction
        {
            return false;
        }
        if let Some(navigation) = decision
            .downcast_ref::<webkit6::NavigationPolicyDecision>()
            .and_then(webkit6::NavigationPolicyDecision::navigation_action)
        {
            let user_gesture = navigation.is_user_gesture();
            if let Some(uri) = navigation.request().and_then(|request| request.uri()) {
                let uri = uri.as_str();
                if user_gesture
                    && (uri.starts_with("http://")
                        || uri.starts_with("https://")
                        || uri.starts_with("mailto:"))
                {
                    let _ = gtk::gio::AppInfo::launch_default_for_uri(
                        uri,
                        gtk::gio::AppLaunchContext::NONE,
                    );
                }
            }
        }
        // Never navigate the reader itself; the message stays on screen.
        decision.ignore();
        true
    });

    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailin_core::html;

    #[test]
    fn head_gets_the_policy_and_default_styles() {
        let document = document(
            "<html><head><title>x</title></head><body>hi</body></html>",
            false,
            false,
            &[],
        );
        assert!(document.contains("Content-Security-Policy"));
        assert!(document.contains("img-src data: cid:;"));
        assert!(document.contains("<title>x</title>"));
        assert!(
            document.find("Content-Security-Policy").unwrap() < document.find("<title>").unwrap()
        );
        assert!(document.contains("script-src 'nonce-"));
    }

    #[test]
    fn remote_content_is_allowed_only_on_request() {
        let blocked = document("<p>x</p>", false, false, &[]);
        assert!(blocked.contains("img-src data: cid:;"));
        let allowed = document("<p>x</p>", true, true, &[]);
        assert!(allowed.contains("img-src data: cid: http: https:;"));
        assert!(allowed.contains("#1e1e1e"));
    }

    #[test]
    fn fragments_are_wrapped_in_a_complete_document() {
        let document = document("<p>Hello</p>", false, false, &[]);
        assert!(document.starts_with("<!DOCTYPE html><html><head>"));
        assert!(document.contains("<body><p>Hello</p></body>"));
    }

    #[test]
    fn cid_images_become_data_urls() {
        let document = document(
            "<img src=\"cid:logo@example.com\"><img src=\"cid:other\">",
            false,
            false,
            &[(
                "logo@example.com".into(),
                "data:image/png;base64,AAAA".into(),
            )],
        );
        assert!(document.contains("src=\"data:image/png;base64,AAAA\""));
        assert!(document.contains("src=\"cid:other\""));
    }

    #[test]
    fn remote_resources_are_detected() {
        assert!(has_remote_content(
            "<p><img src=\"https://tracker.example/pixel.png\"></p>"
        ));
        assert!(has_remote_content(
            "<style>body { background: url(http://example.com/x) }</style>"
        ));
        assert!(has_remote_content(
            "<img src=\"//tracker.example/pixel.png\">"
        ));
        assert!(has_remote_content(
            "<style>body { background: url(//example.com/x) }</style>"
        ));
        assert!(!has_remote_content("<p>See https://example.com</p>"));
        assert!(!has_remote_content("<img src=\"cid:logo\">"));
        assert!(!has_remote_content(
            "<a href=\"https://example.com\">link</a>"
        ));
    }

    #[test]
    fn data_urls_are_base64() {
        assert_eq!(
            html::data_url("image/png", b"PNG"),
            "data:image/png;base64,UE5H"
        );
    }
}
