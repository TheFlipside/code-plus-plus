//! Reading an appcast feed for the signatures it carries (DESIGN.md §11.4).
//!
//! The Windows updater's library verifies a download and then leaves it
//! where it can be rewritten, so Code++ verifies the file again itself
//! before running it. The library hands over a path and nothing else, so
//! the signature to verify against comes from the feed, which Code++
//! fetches a second time. This module turns that feed's text into the
//! signatures it holds. It makes no request and verifies nothing: the
//! platform layer does both.
//!
//! A feed is untrusted input. On Windows nothing signs it, and whoever
//! controls it chooses every byte. So the read is bounded, an item that
//! is not what this module expects is passed over rather than trusted or
//! reported, and nothing read here is shown to the user or put in a
//! command line. What a feed *can* do is withhold the right signature,
//! which refuses the update; it cannot make a file verify that the
//! project's key did not sign.
//!
//! The format is Sparkle's appcast, an RSS document:
//!
//! ```xml
//! <rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
//!   <channel>
//!     <item>
//!       <sparkle:version>0.2.1</sparkle:version>
//!       <enclosure url="https://…/codepp-windows-x64-0.2.1.msi"
//!                  sparkle:edSignature="…base64…" length="4521984"/>
//!     </item>
//!   </channel>
//! </rss>
//! ```

use serde::de::IgnoredAny;
use serde::Deserialize;

/// Length of an Ed25519 public key, in bytes. `codepp_platform`, which
/// `core` cannot name, declares this and [`SIGNATURE_LEN`] again for its
/// verification; that takes arrays of its own lengths, so the two
/// cannot differ without the call between them failing to compile.
pub const PUBLIC_KEY_LEN: usize = 32;

/// Length of an Ed25519 signature, in bytes.
pub const SIGNATURE_LEN: usize = 64;

/// The most bytes of a feed that are read. A feed with a few dozen
/// releases is a few kilobytes.
pub const MAX_FEED_BYTES: u64 = 256 * 1024;

/// The most signed items a feed yields. Each costs the caller one
/// verification of the whole downloaded file, so this bounds what a
/// feed stuffed with items can make it do.
pub const MAX_FEED_ITEMS: usize = 32;

/// The longest version an item may state, in bytes. Longer is not a
/// version, and the item is passed over, as is one whose version holds
/// a control character or one that reorders text.
pub const MAX_VERSION_BYTES: usize = 64;

/// One release a feed offers: the version it states and the signature
/// of its download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedItem {
    /// The version the feed states for this release. The feed's claim,
    /// and nothing more: the caller trusts the version it reads from
    /// the verified file, not this.
    pub version: String,
    /// The Ed25519 signature of the release's download.
    pub signature: [u8; SIGNATURE_LEN],
}

/// Errors from reading a feed.
#[derive(Debug)]
pub enum FeedError {
    /// The read failed, or the bytes are not UTF-8.
    Io(std::io::Error),
    /// The feed is larger than [`MAX_FEED_BYTES`].
    TooLarge,
    /// The document does not parse as a feed.
    Parse(quick_xml::DeError),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "update feed I/O: {e}"),
            Self::TooLarge => write!(f, "update feed is larger than {MAX_FEED_BYTES} bytes"),
            // The parser's message can quote the document, which is not
            // ours; callers log this, so it is cut short and what could
            // reorder or break a log line is replaced.
            Self::Parse(e) => {
                let text = e.to_string();
                let cut: String = text
                    .chars()
                    .take(200)
                    .map(|c| {
                        if crate::display::is_display_hostile(c) {
                            char::REPLACEMENT_CHARACTER
                        } else {
                            c
                        }
                    })
                    .collect();
                write!(f, "update feed parse: {cut}")
            }
        }
    }
}

impl std::error::Error for FeedError {}

// The document is read as lists of children, each an enum by element
// name, and not as structs with one field per element. A struct refuses
// a document that repeats an element it holds once, or that puts
// another element between two of a list, and either would cost the
// whole feed over something harmless. Read this way, an element this
// module does not use is one `Other`, wherever and however often it
// appears.
//
// quick-xml matches a name without its namespace prefix, so `version`
// is `<sparkle:version>` and `@edSignature` is `sparkle:edSignature`.
// Any prefix matches, and none: the feed's namespace is not checked.
// Nothing rests on it, since a signature read from the wrong attribute
// verifies nothing.

#[derive(Deserialize)]
struct Rss {
    #[serde(rename = "$value", default)]
    children: Vec<RssChild>,
}

#[derive(Deserialize)]
enum RssChild {
    #[serde(rename = "channel")]
    Channel(Channel),
    #[serde(rename = "$text")]
    Text(IgnoredAny),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Channel {
    #[serde(rename = "$value", default)]
    children: Vec<ChannelChild>,
}

#[derive(Deserialize)]
enum ChannelChild {
    #[serde(rename = "item")]
    Item(Item),
    #[serde(rename = "$text")]
    Text(IgnoredAny),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "$value", default)]
    children: Vec<ItemChild>,
}

#[derive(Deserialize)]
enum ItemChild {
    /// `<sparkle:version>`. Read as anything, and used only when it is
    /// plain text, so that a version written some other way costs its
    /// item and not the feed.
    #[serde(rename = "version")]
    Version(VersionText),
    #[serde(rename = "enclosure")]
    Enclosure(Enclosure),
    #[serde(rename = "$text")]
    Text(IgnoredAny),
    #[serde(other)]
    Other,
}

/// The content of a `<sparkle:version>` element, as the list of its
/// parts, so that any content reads: text beside elements, in any order.
#[derive(Deserialize)]
struct VersionText {
    #[serde(rename = "$value", default)]
    parts: Vec<VersionPart>,
}

#[derive(Deserialize)]
enum VersionPart {
    #[serde(rename = "$text")]
    Text(String),
    #[serde(other)]
    Other,
}

impl VersionText {
    /// The element's text when text is all it holds, and nothing
    /// otherwise: a version with an element inside it is not one this
    /// module reads.
    fn plain(self) -> Option<String> {
        let mut text = String::new();
        for part in self.parts {
            match part {
                VersionPart::Text(t) => text.push_str(&t),
                VersionPart::Other => return None,
            }
        }
        Some(text)
    }
}

#[derive(Deserialize)]
struct Enclosure {
    /// `sparkle:version` as an attribute, the older place for it.
    #[serde(rename = "@version", default)]
    version: Option<String>,
    #[serde(rename = "@edSignature", default)]
    signature: Option<String>,
}

impl Item {
    /// The release this item offers, if it is one this module can use:
    /// its first enclosure with a signature that decodes, and the
    /// version the item states, as an element first, else on that
    /// enclosure.
    fn release(self) -> Option<FeedItem> {
        let mut element_version = None;
        let mut signed = None;
        for child in self.children {
            match child {
                ItemChild::Version(v) if element_version.is_none() => {
                    element_version = v.plain();
                }
                ItemChild::Enclosure(e) if signed.is_none() => {
                    if let Some(signature) = e.signature.as_deref().and_then(decode_signature) {
                        signed = Some((signature, e.version));
                    }
                }
                _ => {}
            }
        }
        let (signature, attribute_version) = signed?;
        let version = element_version.or(attribute_version)?;
        let version = version.trim();
        // A version is short and plain. One that is neither is no
        // version, and since a caller may log what a feed stated, one
        // holding a character that could reorder or break a line is
        // refused here and not left for each caller to clean.
        if version.is_empty()
            || version.len() > MAX_VERSION_BYTES
            || version.chars().any(crate::display::is_display_hostile)
        {
            return None;
        }
        Some(FeedItem {
            version: version.to_owned(),
            signature,
        })
    }
}

/// The signed releases in the feed `xml`, in the feed's order, at most
/// [`MAX_FEED_ITEMS`] of them.
///
/// An item is taken when it states a version, as an element or on its
/// enclosure, the element first, and one of its enclosures carries a
/// signature that decodes to [`SIGNATURE_LEN`] bytes. Any other item is
/// passed over: a feed may hold items for another platform, or ones in
/// a form this does not read, and neither is this module's to judge.
/// So is every element this does not use, wherever it stands.
///
/// # Errors
///
/// [`FeedError::Parse`] when the document is not well-formed XML.
pub fn parse(xml: &str) -> Result<Vec<FeedItem>, FeedError> {
    let rss: Rss = quick_xml::de::from_str(xml).map_err(FeedError::Parse)?;
    Ok(rss
        .children
        .into_iter()
        .filter_map(|child| match child {
            RssChild::Channel(channel) => Some(channel.children),
            _ => None,
        })
        .flatten()
        .filter_map(|child| match child {
            ChannelChild::Item(item) => item.release(),
            _ => None,
        })
        .take(MAX_FEED_ITEMS)
        .collect())
}

/// [`parse`] a feed read from `reader`, of which at most
/// [`MAX_FEED_BYTES`] are read.
///
/// # Errors
///
/// [`FeedError::Io`] when the read fails or the bytes are not UTF-8,
/// [`FeedError::TooLarge`] past the cap, and [`FeedError::Parse`] when
/// the document does not parse.
pub fn read_from(reader: impl std::io::Read) -> Result<Vec<FeedItem>, FeedError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    reader
        .take(MAX_FEED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(FeedError::Io)?;
    if bytes.len() as u64 > MAX_FEED_BYTES {
        return Err(FeedError::TooLarge);
    }
    let text = String::from_utf8(bytes)
        .map_err(|e| FeedError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    parse(&text)
}

/// Decode an Ed25519 public key as both updater libraries take it: the
/// base64 of its [`PUBLIC_KEY_LEN`] bytes.
#[must_use]
pub fn decode_public_key(text: &str) -> Option<[u8; PUBLIC_KEY_LEN]> {
    decode_base64(text.trim())?.try_into().ok()
}

/// Decode an Ed25519 signature as a feed carries it: the base64 of its
/// [`SIGNATURE_LEN`] bytes.
#[must_use]
pub fn decode_signature(text: &str) -> Option<[u8; SIGNATURE_LEN]> {
    decode_base64(text.trim())?.try_into().ok()
}

/// Decode standard base64 (RFC 4648 §4), strictly: the alphabet
/// `A-Z a-z 0-9 + /`, a length that is a multiple of four, `=` padding
/// only at the end and only as much as the length calls for, and the
/// unused bits of the last character zero. Anything else is `None`.
///
/// Strict because the two things decoded here are a key and a
/// signature, where one text should mean one value.
#[must_use]
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let groups = bytes.len() / 4;
    for (i, group) in bytes.chunks_exact(4).enumerate() {
        let last = i + 1 == groups;
        let padding = group.iter().rev().take_while(|&&c| c == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut word = 0u32;
        for &c in &group[..4 - padding] {
            word = (word << 6) | value(c)?;
        }
        // `word` holds the group's bits in its low end, so the bytes
        // wanted are the last of its big-endian form.
        match padding {
            0 => out.extend_from_slice(&word.to_be_bytes()[1..]),
            1 => {
                // Three characters carry 18 bits for 2 bytes; the low 2
                // must be zero.
                if word & 0b11 != 0 {
                    return None;
                }
                out.extend_from_slice(&(word >> 2).to_be_bytes()[2..]);
            }
            _ => {
                // Two characters carry 12 bits for 1 byte; the low 4
                // must be zero.
                if word & 0b1111 != 0 {
                    return None;
                }
                out.extend_from_slice(&(word >> 4).to_be_bytes()[3..]);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signature `winsparkle-tool` 0.9.4 printed for a test file,
    /// and the first and last of its 64 bytes.
    const SIGNATURE: &str =
        "Fxn//dBhrNnpfjRj2VXEMUIYEf3JxbirV6oYUtkpof5zzeunuZtuzUJ5N4OOpTA7sJuSjKkT5qoUfIpTVrU1BA==";
    const SIGNATURE_ENDS: (u8, u8) = (0x17, 0x04);
    /// The public key it printed, and the first and last of its 32 bytes.
    const PUBLIC_KEY: &str = "GfY7YUA2b3vvIlE+0mmQzGWrS+73SQL/6/68MxGvhvg=";
    const PUBLIC_KEY_ENDS: (u8, u8) = (0x19, 0xF8);

    fn feed(items: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <rss version=\"2.0\" \
             xmlns:sparkle=\"http://www.andymatuschak.org/xml-namespaces/sparkle\">\
             <channel><title>Code++</title>{items}</channel></rss>"
        )
    }

    #[test]
    fn base64_decodes_the_rfc_4648_vectors() {
        for (text, bytes) in [
            ("", &b""[..]),
            ("Zg==", b"f"),
            ("Zm8=", b"fo"),
            ("Zm9v", b"foo"),
            ("Zm9vYg==", b"foob"),
            ("Zm9vYmE=", b"fooba"),
            ("Zm9vYmFy", b"foobar"),
            ("+/+/", &[0xFB, 0xFF, 0xBF]),
        ] {
            assert_eq!(decode_base64(text).as_deref(), Some(bytes), "{text}");
        }
    }

    #[test]
    fn base64_refuses_what_is_not_strict() {
        for text in [
            "Zg",        // no padding
            "Zg=",       // short padding
            "Zg===",     // not a multiple of four
            "Z===",      // three padding characters
            "====",      // nothing but padding
            "Zg==Zg==",  // padding before the end
            "Zm9v Zg==", // whitespace inside
            "Zm9v\n",    // a line end
            "Zm-_",      // the URL alphabet
            "Zh==",      // unused bits set (2 characters)
            "Zm9=",      // unused bits set (3 characters)
            "Zm9\u{e9}", // not ASCII, and five bytes
            "Zm\u{e9}",  // not ASCII, in four bytes
        ] {
            assert_eq!(decode_base64(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_key_and_a_signature_decode_to_their_lengths_only() {
        let key = decode_public_key(PUBLIC_KEY).expect("key");
        assert_eq!((key[0], key[31]), PUBLIC_KEY_ENDS);
        let signature = decode_signature(SIGNATURE).expect("signature");
        assert_eq!((signature[0], signature[63]), SIGNATURE_ENDS);
        assert_eq!(
            decode_public_key(&format!("  {PUBLIC_KEY}\r\n")),
            Some(key),
            "space around the text is not part of it"
        );
        assert_eq!(decode_public_key(SIGNATURE), None, "64 bytes are no key");
        assert_eq!(
            decode_signature(PUBLIC_KEY),
            None,
            "32 bytes are no signature"
        );
        assert_eq!(decode_public_key(""), None);
        assert_eq!(decode_signature("not base64 at all"), None);
    }

    /// The two places a feed states a version, and the element first.
    #[test]
    fn an_item_is_read_with_its_version_in_either_place() {
        let xml = feed(&format!(
            "<item><title>0.2.1</title><sparkle:version>0.2.1</sparkle:version>\
             <enclosure url=\"https://example.org/a.msi\" sparkle:edSignature=\"{SIGNATURE}\" \
             length=\"1\" type=\"application/octet-stream\"/></item>\
             <item><enclosure url=\"https://example.org/b.msi\" sparkle:version=\"0.2.0\" \
             sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             <item><sparkle:version> 0.1.9 </sparkle:version>\
             <enclosure sparkle:version=\"9.9.9\" sparkle:edSignature=\"{SIGNATURE}\"/></item>"
        ));
        let items = parse(&xml).expect("parse");
        let versions: Vec<&str> = items.iter().map(|i| i.version.as_str()).collect();
        assert_eq!(versions, ["0.2.1", "0.2.0", "0.1.9"]);
        for item in &items {
            assert_eq!(
                (item.signature[0], item.signature[63]),
                SIGNATURE_ENDS,
                "{}",
                item.version
            );
        }
    }

    /// Items this module cannot use are passed over, and the ones after
    /// them still read.
    #[test]
    fn an_item_without_a_usable_version_or_signature_is_passed_over() {
        let long = "9".repeat(MAX_VERSION_BYTES + 1);
        let xml = feed(&format!(
            "<item><sparkle:version>1</sparkle:version></item>\
             <item><sparkle:version>2</sparkle:version><enclosure url=\"u\"/></item>\
             <item><sparkle:version>3</sparkle:version>\
             <enclosure sparkle:edSignature=\"{PUBLIC_KEY}\"/></item>\
             <item><sparkle:version>4</sparkle:version>\
             <enclosure sparkle:edSignature=\"not base64\"/></item>\
             <item><enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             <item><sparkle:version>  </sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             <item><sparkle:version>{long}</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             <item><sparkle:version>8</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>"
        ));
        let items = parse(&xml).expect("parse");
        let versions: Vec<&str> = items.iter().map(|i| i.version.as_str()).collect();
        assert_eq!(versions, ["8"]);
    }

    /// A feed as a release tool writes one, with everything this module
    /// does not use: it reads, and gives its one signed item.
    #[test]
    fn a_realistic_feed_reads() {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <rss version=\"2.0\" \
             xmlns:sparkle=\"http://www.andymatuschak.org/xml-namespaces/sparkle\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n  <channel>\n    \
             <title>Code++</title>\n    <link>https://example.org/appcast.xml</link>\n    \
             <description>Most recent changes.</description>\n    <language>en</language>\n    \
             <item>\n      <title>Version 0.2.1</title>\n      \
             <description><![CDATA[<h2>New</h2><ul><li>a &amp; b</li></ul>]]></description>\n      \
             <pubDate>Fri, 13 Nov 2026 10:00:00 +0000</pubDate>\n      \
             <sparkle:releaseNotesLink>https://example.org/notes.html</sparkle:releaseNotesLink>\n      \
             <sparkle:version>0.2.1</sparkle:version>\n      \
             <sparkle:shortVersionString>0.2.1</sparkle:shortVersionString>\n      \
             <sparkle:minimumSystemVersion>10.0</sparkle:minimumSystemVersion>\n      \
             <enclosure url=\"https://example.org/codepp-0.2.1.msi\" sparkle:os=\"windows\" \
             sparkle:edSignature=\"{SIGNATURE}\" length=\"4521984\" \
             type=\"application/octet-stream\"/>\n    </item>\n  </channel>\n</rss>\n"
        );
        let items = parse(&xml).expect("parse");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].version, "0.2.1");
        assert_eq!(
            (items[0].signature[0], items[0].signature[63]),
            SIGNATURE_ENDS
        );
    }

    /// Variations a struct-shaped reader would refuse the whole feed
    /// over: an element between two items, an item with two enclosures
    /// or two versions, a second channel, text where none is expected,
    /// a version that is an element or has one amid its text, and an
    /// enclosure with content. Each costs at most its own item.
    #[test]
    fn a_harmless_variation_does_not_cost_the_feed() {
        let good = |v: &str| {
            format!(
                "<item><sparkle:version>{v}</sparkle:version>\
                 <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>"
            )
        };
        let xml = format!(
            "<rss version=\"2.0\" xmlns:sparkle=\"x\"><channel>{}<ttl>60</ttl>{}\
             <item><enclosure url=\"mac.zip\"/><sparkle:version>3</sparkle:version>\
             <title>t</title><enclosure sparkle:edSignature=\"{SIGNATURE}\"/>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\" sparkle:version=\"9\"/></item>\
             <item><sparkle:version>4</sparkle:version><title>t</title>\
             <sparkle:version>5</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             stray text\
             <item><sparkle:version><b>6</b></sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             <item><sparkle:version>6<b/>.5</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             <item><sparkle:version>6.6<!-- c --></sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\">text<x/></enclosure></item>\
             <item>text in an item<sparkle:version>7</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             </channel><other/><channel>{}</channel></rss>",
            good("1"),
            good("2"),
            good("8"),
        );
        let items = parse(&xml).expect("parse");
        let versions: Vec<&str> = items.iter().map(|i| i.version.as_str()).collect();
        assert_eq!(versions, ["1", "2", "3", "4", "6.6", "7", "8"]);
    }

    /// What may stand around and on the elements this module reads: a
    /// byte-order mark, a declaration, a doctype, a processing
    /// instruction, comments, and attributes on every element.
    #[test]
    fn a_prolog_comments_and_attributes_are_read_past() {
        let xml = format!(
            "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <!DOCTYPE rss><?xml-stylesheet href=\"a.css\"?><!-- a feed -->\
             <rss version=\"2.0\" xmlns:sparkle=\"x\"><channel id=\"c\"><!-- items -->\
             <item id=\"i\"><sparkle:version kind=\"k\">0.2.1</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
             <sparkle:item><sparkle:version>9</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></sparkle:item>\
             </channel></rss>"
        );
        let items = parse(&xml).expect("parse");
        let versions: Vec<&str> = items.iter().map(|i| i.version.as_str()).collect();
        assert_eq!(
            versions,
            ["0.2.1", "9"],
            "a prefix is not part of a name here, so `sparkle:item` is an item"
        );
    }

    /// A version that could reorder or break a log line is no version.
    #[test]
    fn a_version_with_a_hostile_character_is_passed_over() {
        for hostile in ["0.2\u{202e}1", "0.2&#x1b;1", "0.2&#10;1", "0.2\u{200b}"] {
            let xml = feed(&format!(
                "<item><sparkle:version>{hostile}</sparkle:version>\
                 <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>\
                 <item><enclosure sparkle:version=\"{hostile}\" \
                 sparkle:edSignature=\"{SIGNATURE}\"/></item>\
                 <item><sparkle:version>ok</sparkle:version>\
                 <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>"
            ));
            let items = parse(&xml).expect("parse");
            let versions: Vec<&str> = items.iter().map(|i| i.version.as_str()).collect();
            assert_eq!(versions, ["ok"], "{hostile:?}");
        }
    }

    #[test]
    fn a_feed_with_no_items_or_no_channel_is_empty() {
        assert_eq!(parse(&feed("")).expect("parse"), []);
        assert_eq!(parse("<rss version=\"2.0\"></rss>").expect("parse"), []);
    }

    #[test]
    fn no_more_items_than_the_cap_are_taken() {
        use std::fmt::Write as _;
        let mut items = String::new();
        for i in 0..MAX_FEED_ITEMS + 5 {
            write!(
                items,
                "<item><sparkle:version>{i}</sparkle:version>\
                     <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>"
            )
            .expect("write to a String");
        }
        let read = parse(&feed(&items)).expect("parse");
        assert_eq!(read.len(), MAX_FEED_ITEMS);
        assert_eq!(read[0].version, "0");
        assert_eq!(
            read[MAX_FEED_ITEMS - 1].version,
            (MAX_FEED_ITEMS - 1).to_string()
        );
    }

    #[test]
    fn what_is_not_a_feed_is_an_error() {
        for xml in [
            "",
            "not xml",
            "<rss><channel>",
            "<rss><channel><item></channel></rss>",
        ] {
            assert!(matches!(parse(xml), Err(FeedError::Parse(_))), "{xml:?}");
        }
        let long = format!("<rss>{}", "x".repeat(5000));
        let message = parse(&long).expect_err("unclosed").to_string();
        assert!(
            message.len() < 300,
            "the message is cut short: {}",
            message.len()
        );
    }

    /// The parser quotes the document in its message, and the document
    /// is not ours: what could reorder or break a log line is replaced
    /// before the message is shown.
    #[test]
    fn a_parse_error_carries_no_hostile_character() {
        let hostile = "<rss><channel><a\u{202e}b></c\u{1b}d></channel></rss>";
        let error = parse(hostile).expect_err("mismatched tags");
        let FeedError::Parse(inner) = &error else {
            panic!("a parse error, not {error:?}");
        };
        let raw = inner.to_string();
        assert!(
            raw.contains('\u{202e}') || raw.contains('\u{1b}'),
            "the parser must quote the document for this test to mean anything: {raw:?}"
        );
        let shown = error.to_string();
        assert!(
            !shown.chars().any(crate::display::is_display_hostile),
            "{shown:?}"
        );
        assert!(shown.contains(char::REPLACEMENT_CHARACTER), "{shown:?}");
    }

    #[test]
    fn a_read_is_bounded_and_takes_utf8_only() {
        let cap = usize::try_from(MAX_FEED_BYTES).expect("fits");
        let empty = feed("");
        let at = format!("{empty}{}", " ".repeat(cap - empty.len()));
        assert_eq!(at.len(), cap);
        assert_eq!(read_from(at.as_bytes()).expect("read"), []);
        let over = format!("{at} ");
        assert!(matches!(
            read_from(over.as_bytes()),
            Err(FeedError::TooLarge)
        ));
        assert!(matches!(
            read_from(&[0xFF, 0xFE, 0x00][..]),
            Err(FeedError::Io(_))
        ));
        let xml = feed(&format!(
            "<item><sparkle:version>0.2.1</sparkle:version>\
             <enclosure sparkle:edSignature=\"{SIGNATURE}\"/></item>"
        ));
        assert_eq!(read_from(xml.as_bytes()).expect("read").len(), 1);
    }
}
