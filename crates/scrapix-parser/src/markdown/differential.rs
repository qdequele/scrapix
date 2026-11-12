//! Differential tests of the Markdown conversion against [`super::legacy`],
//! the implementation before SCR-90.
//!
//! The Markdown is indexed and returned by `/scrape`: a faster conversion
//! must give the same bytes, or a re-crawl rewrites every document. Every
//! public entry point and every internal stage is run through both
//! implementations over a corpus of
//!
//! - the HTML string literals of the workspace's Rust sources (tests,
//!   benches, fixtures of every crate),
//! - hand-written pages around the tags the conversion strips,
//! - seeded random pages built from the same vocabulary,
//! - the bench inputs.

use super::legacy;
use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// =============================================================================
// Corpus
// =============================================================================

/// A small deterministic generator (xorshift64*), so the random pages are
/// the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Tags the conversion strips, tags it could confuse with them, and other
/// containers.
const TAG_NAMES: &[&str] = &[
    "script",
    "style",
    "noscript",
    "svg",
    "head",
    "nav",
    "footer",
    "header",
    "aside",
    "iframe",
    "form",
    "SCRIPT",
    "Style",
    "NoScript",
    "SVG",
    "HEAD",
    "Nav",
    "FOOTER",
    "Header",
    "ASIDE",
    "scripty",
    "styles",
    "navigation",
    "headers",
    "heading",
    "footers",
    "asides",
    "svgs",
    "\u{17f}cript",
    "\u{17f}tyle",
    "a\u{17f}ide",
    "div",
    "main",
    "article",
    "section",
    "p",
    "span",
];

/// What follows a tag name in an opening tag.
const OPEN_TAILS: &[&str] = &[
    ">",
    ">",
    ">",
    " >",
    " class=\"x\">",
    "\n>",
    "\t id='a'>",
    " data-x=\"<nav>\">",
    " data-y='</script>'>",
    "/>",
    " />",
    "\u{a0}>",
    "\u{2003}class=\"y\">",
    "",
    "x>",
    "-custom>",
];

/// What follows a tag name in a closing tag.
const CLOSE_TAILS: &[&str] = &[
    ">", ">", ">", ">", " >", "\n>", "\t\n >", "\u{a0}>", " x>", "", "/>",
];

/// Content between tags.
const FRAGMENTS: &[&str] = &[
    "plain text ",
    "Some words, and   several   spaces. ",
    "<p>A paragraph with <strong>bold</strong> and <em>italic</em>.</p>",
    "<h1>Title</h1>",
    "<h2>#Not spaced</h2>",
    "<h3>Sub <a href=\"/s\">heading</a></h3>",
    "<a href=\"/relative\">relative</a>",
    "<a href=\"relative/path?q=1#frag\">deep</a>",
    "<a href=\"../up\">up</a>",
    "<a href=\"https://example.org/abs\">absolute</a>",
    "<a href=\"//cdn.example.org/x\">scheme relative</a>",
    "<a href=\"mailto:me@example.org\">mail</a>",
    "<a href=\"#top\">top</a>",
    "<a href=\"javascript:void(0)\">js</a>",
    "<a href=\"MAILTO:Me@Example.ORG\">upper mail</a>",
    "<a href=\"http:rel\">http rel</a> <a href=\"https:rel\">https rel</a>",
    "<a href=\"HTTP://Example.ORG:80/a/../b\">normalizable</a>",
    "<a href=\"/with space\">spaced target</a>",
    "<a href=\"/t\" title=\"A title\">titled</a>",
    "<a href=\"/paren(1)\">paren</a>",
    "<a>no href</a>",
    "<a href=\"\">empty href</a>",
    "<a href=\"https://github.com\"></a>",
    "<a href=\"/img\"><img src=\"logo.png\" alt=\"Logo\"></a>",
    "<a href=\"/multi\">\n\n  Multi line text\n\n</a>",
    "<a href=\"/block\"><div>Block</div><p>in a link</p></a>",
    "<img src=\"/_next/static/media/bg.svg\" alt=\"\">",
    "<img src=\"/_next/image?url=a.png\" alt=\"Decorative\">",
    "<img src=\"photo.jpg\" alt=\"A photo\">",
    "<img src=\"https://example.org/p.png\">",
    "<img alt=\"only alt\">",
    "<IMG SRC=\"UP.PNG\" ALT=\"Upper\">",
    "<ul><li>One</li><li>Two <a href=\"two\">2</a></li></ul>",
    "<ol><li>First</li><li>Second</li></ol>",
    "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td><a href=\"/c\">2</a></td></tr></table>",
    "<table><tr><td><img src=\"t.png\" alt=\"in table\"></td><td>x | y</td></tr></table>",
    "<pre><code>fn main() { let x = a &lt; b &amp;&amp; c &gt; d; }</code></pre>",
    "<pre>pre   text\n    indented [a](/b)\n```\nfence\n</pre>",
    "<code>inline `code` [x](/y)</code>",
    "<pre><code>&lt;script&gt;alert(1)&lt;/script&gt;</code></pre>",
    "<blockquote>Quoted <cite>source</cite></blockquote>",
    "&lt;nav&gt; as text &amp; entities &nbsp; &copy; &#169; &#x1F600; &#xD800; &#0; &bogus; ",
    "1 < 2 and 3 > 2, a<b>c ",
    "text with [brackets](and parens) and ![bang](x.png) ",
    "[\n\nloose\n\n](/loose) ",
    "* star _under_ ~tilde~ \\ backslash # hash - dash + plus = eq > gt ",
    "<!-- a comment with <script>in it</script> and </nav> -->",
    "<!-- unclosed comment <footer>",
    "<![CDATA[ <style>x</style> ]]>",
    "<sub>sub</sub><sup>sup</sup>",
    "<details><summary>More</summary>Hidden <b>text</b></details>",
    "<iframe src=\"https://www.youtube.com/embed/abc\"></iframe>",
    "<iframe src=\"/embed\" width=\"10\"></iframe>",
    "<form action=\"/f\"><input name=\"q\"><button>Go</button></form>",
    "<br>",
    "<hr>",
    "\n\n\n\n",
    "   \t  ",
    "日本語のテキスト ",
    "émoji 😀 and \u{17f} long s ",
    "\u{212a}elvin \u{a0}nbsp\u{2028}line sep ",
    "var s = '</style>'; if (a<b) { x = \"<nav>\"; } ",
    "body{margin:0} .a>b{color:red} ",
    "<path d=\"M0 0L1 1\"/><circle r=\"1\"/>",
    "<title>A title</title><meta name=\"d\" content=\"x\"><base href=\"/base/\">",
    "<base href=\"https://other.example/root/\">",
    "</p>",
    "</div></div>",
    "<",
    ">",
    "</",
    "<<nav>>",
    "<na",
    "v>",
    "</na",
    "<scr",
    "ipt>",
    "</scr",
];

/// A random page: opening and closing tags of the vocabulary around the
/// fragments, deliberately unbalanced, interleaved and split.
fn random_page(rng: &mut Rng) -> String {
    let mut page = String::new();
    let pieces = 1 + rng.below(40);
    for _ in 0..pieces {
        match rng.below(10) {
            0..=2 => {
                page.push('<');
                page.push_str(rng.pick(TAG_NAMES));
                page.push_str(rng.pick(OPEN_TAILS));
            }
            3..=5 => {
                page.push_str("</");
                page.push_str(rng.pick(TAG_NAMES));
                page.push_str(rng.pick(CLOSE_TAILS));
            }
            6 => {
                // A balanced block.
                let tag = rng.pick(TAG_NAMES);
                page.push_str(&format!(
                    "<{tag}{}{}</{tag}{}",
                    rng.pick(OPEN_TAILS),
                    rng.pick(FRAGMENTS),
                    rng.pick(CLOSE_TAILS)
                ));
            }
            7 => {
                // A tag name split around a block: removing the block
                // joins the halves into a tag.
                let name = rng.pick(&TAG_NAMES[..11]);
                let inner = rng.pick(&TAG_NAMES[..11]);
                let split = rng.below(name.len() + 1);
                page.push_str(if rng.below(2) == 0 { "<" } else { "</" });
                page.push_str(&name[..split]);
                page.push_str(&format!("<{inner}>{}</{inner}>", rng.pick(FRAGMENTS)));
                page.push_str(&name[split..]);
                page.push_str(rng.pick(OPEN_TAILS));
            }
            _ => page.push_str(rng.pick(FRAGMENTS)),
        }
    }
    page
}

/// Pages written by hand around the known traps.
fn handwritten_pages() -> Vec<String> {
    let mut pages: Vec<String> = [
        "",
        " ",
        "\n",
        "plain text, no tag at all",
        "<p>plain paragraph, nothing to strip</p>",
        "<html><head><title>t</title><style>a{}</style></head><body><p>x</p></body></html>",
        "<body><SCRIPT type='a'>var s = '</style>';</SCRIPT><p>kept</p><style>p{}</style></body>",
        // Interleaved: the order of the passes decides the result.
        "<style>a{} <script>b</style> c</script><p>interleaved</p>",
        "<script>a <style>b</script> c</style><p>interleaved the other way</p>",
        "<nav>a <script>b</nav> c</script><p>nav around script</p>",
        "<script>a <nav>b</script> c</nav><p>script around nav</p>",
        "<header>a <footer>b</header> c</footer> d <aside>e <nav>f</aside> g</nav> h",
        "<head>a <svg>b</head> c</svg> d <noscript>e <head>f</noscript> g</head> h",
        // Nested: the lazy match stops at the first closing tag.
        "<nav><nav>inner</nav>between</nav>after",
        "<aside><aside><aside>x</aside>y</aside>z</aside>w",
        "<script><script>x</script>y</script>z",
        // A removal joins two halves into a new tag.
        "<na<script>x</script>v>menu</nav><p>joined opening</p>",
        "<nav>menu</na<style>x</style>v><p>joined closing</p>",
        "<sc<script>x</script>ript>y</script><p>joined same tag</p>",
        "<hea<nav>x</nav>der>title</header>tail",
        "<foo<head>x</head>ter>f</footer>tail",
        "<asi<svg>x</svg>de>f</aside>tail <st<noscript>x</noscript>yle>g</style>tail",
        // Unclosed, unopened.
        "<script>unclosed <p>text</p>",
        "<p>text</p></script><nav>unclosed nav",
        "</nav><nav>a</nav></nav>",
        "<nav>a<nav>b",
        "<script",
        "<script>",
        "</script>",
        "<script></script>",
        "<script ></script >",
        "<script\n></script\n\t >",
        "<script/>x</script>",
        "<script></script x>y</script>",
        "<script></script/>y</script>",
        // Near misses.
        "<navigation>not a nav</navigation><headers>nor a header</headers>",
        "<scripty>x</scripty><styles>y</styles><svgs>z</svgs><asides>w</asides>",
        "<header>title</header><head>h</head><heading>x</heading>",
        "<head>h</head><header>title</header>",
        "<header>a</head>b</header>c",
        "<head>a</header>b</head>c",
        "<nav-item>x</nav-item><nav>y</nav>",
        "< nav>x</nav>",
        "<nav>x</ nav>y</nav>",
        // Case and Unicode.
        "<NAV>x</nav><Nav>y</NAV><nAv CLASS='a'>z</NaV >",
        "<\u{17f}cript>long s</\u{17f}cript><p>kept</p>",
        "<script>a</\u{17f}cript>b</script>c",
        "<a\u{17f}ide>long s aside</aside>tail <nav>n</nav>",
        "<\u{212a}>kelvin</\u{212a}><nav\u{a0}class>nbsp</nav\u{2003}>tail",
        "<nav\u{3000}>ideographic space</nav\u{feff}>bom is not space</nav>",
        "<navé>x</nav>y <nav>é</navé>z</nav>",
        // Tags in attributes, comments and scripts.
        "<div data-x=\"<nav>\">a</div><p title='</nav>'>b</p>",
        "<!-- <script> --><p>shown?</p><!-- </script> -->",
        "<script>document.write('<nav>x</nav>')</script><nav>real</nav>",
        "<script>var a = '</scr' + 'ipt>';</script><p>after</p>",
        "<svg><style>.a{}</style><path d=''/></svg><p>after svg</p>",
        "<nav><svg><path d=''/></svg></nav><p>text</p><noscript><img src='p.gif'></noscript>",
        "<nav>menu</nav><header><h1>T</h1></header><main>body</main><aside>x</aside><footer>f</footer>",
        // Markdown post-processing.
        "<h1>#hash title</h1><h2>##</h2><h3># #</h3><p>#tag</p><p>#</p>",
        "<p>a  b   c    d</p><pre>a  b   c</pre><p>trailing   </p>",
        "<p>```</p><p>a  b</p><p>```</p><p>c  d</p>",
        "<pre>```\ninside  spaces\n```</pre><p>after  fence</p>",
        "<p>~~~</p><a href=\"/x\">in tilde fence?</a><p>~~~</p><a href=\"/y\">out</a>",
        "<a href=\"/a\">one</a> `<a href=\"/b\">two</a>` <a href=\"/c\">three</a>",
        "<p>`</p><a href=\"/odd\">odd backtick</a>",
        "<a href=\"/x\">\n\n<img src=\"a.png\" alt=\"A\">\n\n</a>",
        "<a href=\"/x\"><img src=\"/_next/a.png\" alt=\"A\"></a><a href=\"/y\"><img src=\"b.png\"></a>",
        "<a href=\"/x\"><h2>Heading link</h2></a><a href=\"/y\"><p>Para</p><p>link</p></a>",
        "<a href=\"\"><img src=\"\" alt=\"\"></a><img src=\"\" alt=\"alt only\"><img>",
        "<a href=\"/a b\">x</a><a href=\"/q?a=1&amp;b=2\">y</a><a href=\"/p)\">z</a>",
        "<a href=\"http://[bad\">bad host</a><a href=\"x:y\">odd scheme</a><a href=\":x\">colon</a>",
        "<a href=\"c:\\path\">drive</a><a href=\"1:2\">digits</a><a href=\"a/b:c\">colon in path</a>",
        "<a href=\"?q=1\">query</a><a href=\"./\">dot</a><a href=\"..\">dotdot</a><a href=\"/\">root</a>",
        "<a href=\"HTTPS://EXAMPLE.ORG/UP\">upper</a><a href=\"data:text/plain,hi\">data</a>",
        "<a href=\"tel:+33123\">tel</a><a href=\" /lead\">leading space</a>",
        "<p>[text](/in-text) and ![img](in-text.png) and [](/empty) and ![](/noalt.png)</p>",
        "<p>[a]( /spaced \"title\" ) [b](<angle>) [c][ref] [d](/d \"t\")</p>",
        "<sub>1</sub><sup>2</sup><details><summary>S</summary>D</details><span>x</span>",
        "<p>1 &lt; 2 &gt; 0, a&lt;b&gt;c, &lt;img src=\"x\" alt=\"y\"&gt;</p>",
        "<pre>&lt;img src=\"a.png\" alt=\"A\"&gt; &lt;b&gt;bold&lt;/b&gt; a &lt; b</pre>",
        "<code>&lt;IMG SRC=\"a.png\"&gt;</code><code>x &lt; y &gt; z</code>",
        "<table><tr><th>H</th></tr><tr><td>&lt;td&gt;</td></tr></table>",
        "<table><tr><td>a</td></tr></table><table><tr><td>b</td></tr></table>",
        "<ul><li><ul><li>nested <a href=\"n\">n</a></li></ul></li></ul>",
        "<iframe src=\"https://www.youtube.com/embed/abc\"></iframe><iframe></iframe>",
        "<form><nav>x</nav><input value=\"<footer>\"></form>",
        "<html><head><base href=\"/app/\"></head><body><a href=\"settings\">S</a></body></html>",
        "<HTML><HEAD><BASE HREF=\"/app/\"></HEAD><BODY><a href=\"s\">S</a></BODY>",
        "<head><base href=\"javascript:alert(1)//\"></head><a href=\"x\">x</a>",
        "<head><base href='https://cdn.example/a/b/'></head><img src=\"i.png\" alt=\"i\">",
        "<body><base href=\"/late/\"><a href=\"x\">base in body</a></body>",
        "&#xD800;&#xDFFF;&#x110000;&#0;&#128;&#x80;&#150;&nbsp;&shy;&zwj;\u{0}\u{1}\u{7f}",
        "\u{feff}<p>bom</p>\r\n<p>crlf</p>\r<p>cr</p>",
        "<p>a\u{2028}b\u{2029}c\u{85}d\u{b}e\u{c}f</p>",
    ]
    .iter()
    .map(|page| page.to_string())
    .collect();

    // Very long lines and many repetitions.
    pages.push(format!("<p>{}</p>", "word ".repeat(20_000)));
    pages.push(format!("<p>{}</p>", "a  b ".repeat(5_000)));
    pages.push(format!("<pre>{}</pre>", "x < y > z ".repeat(5_000)));
    pages.push("<nav>x</nav>".repeat(500));
    pages.push("<nav>".repeat(500) + &"</nav>".repeat(500));
    pages.push("<script>".repeat(300) + "x" + &"</style>".repeat(300));
    pages.push("<a href=\"/l\">link</a> ".repeat(2_000));
    pages.push("<".repeat(3_000) + &">".repeat(3_000));
    pages.push(format!(
        "<script>{}</script><p>after a big script</p>",
        "var x = '<nav>';\n".repeat(3_000)
    ));
    pages.push(bench_large_html(100, 500));
    pages.push(bench_large_html(7, 3));
    pages.push(bench_realistic_html(60));
    pages.push(bench_realistic_html(3));
    // No lowercase </head> or <body>, more than 16 KiB, a multi-byte char
    // across byte 16384.
    let mut long_head = String::from("<HTML><HEAD><TITLE>t</TITLE></HEAD><BODY>");
    while long_head.len() < 16 * 1024 - 1 {
        long_head.push('a');
    }
    long_head.push_str("日本語のテキスト<a href=\"x\">x</a></BODY></HTML>");
    pages.push(long_head);
    pages
}

/// `generate_large_html` of `benches/parser_benchmarks.rs`.
fn bench_large_html(paragraphs: usize, links: usize) -> String {
    let mut html = String::from(
        "\n<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n    <title>Large Document</title>\n    \
         <meta name=\"description\" content=\"A large test document\">\n</head>\n<body>\n    \
         <header><nav><a href=\"/\">Home</a></nav></header>\n    <main>\n        <article>\n            \
         <h1>Large Document Title</h1>\n",
    );
    for i in 0..paragraphs {
        html.push_str(&format!(
            "\n            <h2>Section {i}</h2>\n            <p>This is paragraph {i} with some content that makes it realistic.\n               \
             It contains multiple sentences and some <strong>bold</strong> and\n               \
             <em>italic</em> text. Lorem ipsum dolor sit amet, consectetur\n               \
             adipiscing elit.</p>\n"
        ));
        if i % 5 == 0 {
            for j in 0..(links / (paragraphs / 5 + 1)) {
                html.push_str(&format!(
                    "            <a href=\"/page{i}_{j}\">Link {j} in section {i}</a>\n"
                ));
            }
        }
    }
    html.push_str(
        "\n        </article>\n    </main>\n    <footer><p>Copyright 2024</p></footer>\n</body>\n</html>\n",
    );
    html
}

/// A page shaped like `generate_realistic_html` of the benches: scripts,
/// styles, an SVG, navigation, sidebar and footer around an article.
fn bench_realistic_html(sections: usize) -> String {
    let mut html = String::from(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Realistic Page</title>\n\
         <base href=\"/docs/\">\n<style>body{margin:0}.nav a{padding:4px 8px}.hero>h1{font-size:2rem}</style>\n\
         <script>window.dataLayer=window.dataLayer||[];function gtag(){dataLayer.push(arguments);}</script>\n\
         <script type=\"application/ld+json\">{\"@type\":\"Article\"}</script>\n</head>\n<body class=\"docs\">\n\
         <header class=\"site-header\"><a href=\"/\" class=\"logo\"><svg viewBox=\"0 0 24 24\"><path d=\"M12 2L2 7z\"/></svg></a>\n\
         <nav class=\"nav\"><a href=\"/\">Home</a><a href=\"/docs\">Docs</a><a href=\"https://github.com/example/repo\">GitHub</a></nav>\n\
         </header>\n<aside class=\"sidebar\"><ul>\n",
    );
    for i in 0..sections {
        html.push_str(&format!(
            "<li><a href=\"#section-{i}\">Section {i}</a></li>\n"
        ));
    }
    html.push_str("</ul></aside>\n<main>\n<article>\n<h1>Realistic Page Title</h1>\n");
    for i in 0..sections {
        html.push_str(&format!(
            "<h2 id=\"section-{i}\">Section {i}</h2>\n<p>Paragraph {i} explains a <a href=\"guide/topic-{i}\">relative topic</a>, an\n   \
             <a href=\"https://example.org/ref/{i}\">external reference</a> and some\n   <strong>bold</strong>, <em>italic</em> and \
             <code>inline_code({i})</code> text.</p>\n<img src=\"img/figure-{i}.png\" alt=\"Figure {i}\">\n"
        ));
        if i % 4 == 0 {
            html.push_str(&format!(
                "<pre><code class=\"language-js\">const x{i} = await fetch(\"/api/items?page={i}&amp;size=10\");\n\
                 if (x{i}.status &lt; 400) {{ console.log(\"ok\"); }}</code></pre>\n\
                 <table><tr><th>Option</th><th>Default</th></tr><tr><td>retries</td><td>{i}</td></tr></table>\n\
                 <ul><li>First point of {i}</li><li>Second point with a <a href=\"/faq#q{i}\">link</a></li></ul>\n"
            ));
        }
        if i % 10 == 0 {
            html.push_str(&format!(
                "<script>document.querySelectorAll('#section-{i} a').forEach(function(a){{a.dataset.s='{i}';}});</script>\n\
                 <noscript><img src=\"/pixel.gif?s={i}\" alt=\"\"></noscript>\n"
            ));
        }
    }
    html.push_str(
        "</article>\n</main>\n<footer class=\"site-footer\"><p>Copyright 2026 Example Inc.</p>\n\
         <a href=\"/privacy\">Privacy</a></footer>\n<script src=\"/assets/app.js\" defer></script>\n\
         <script>(function(){var s=document.createElement('script');document.head.appendChild(s);})();</script>\n\
         </body>\n</html>\n",
    );
    html
}

/// The Rust sources of the workspace (`crates/`, `bins/`, `tests/`,
/// `benches/`), or nothing when the crate is built outside it.
fn workspace_sources() -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "target" && !name.starts_with('.') {
                    visit(&path, out);
                }
            } else if name.ends_with(".rs") {
                out.push(path);
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    for dir in ["crates", "bins", "tests", "benches"] {
        visit(&root.join(dir), &mut out);
    }
    out
}

/// The string literals of a Rust source that look like HTML: raw strings
/// as they are, plain strings with their common escapes resolved. A rough
/// lexer is enough, the result only has to be varied HTML.
fn html_literals(source: &str) -> Vec<String> {
    static RAW_RE: OnceLock<Regex> = OnceLock::new();
    static PLAIN_RE: OnceLock<Regex> = OnceLock::new();
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let plain_re = PLAIN_RE.get_or_init(|| Regex::new(r#"(?s)"((?:[^"\\]|\\.)*)""#).unwrap());
    let raw_re = RAW_RE.get_or_init(|| {
        Regex::new(r####"(?s)r###"(.*?)"###|r##"(.*?)"##|r#"(.*?)"#|r"([^"]*)""####).unwrap()
    });
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"</?[a-zA-Z][^<>]*>").unwrap());

    let mut literals = Vec::new();
    for caps in raw_re.captures_iter(source) {
        let literal = (1..=4).find_map(|i| caps.get(i)).unwrap().as_str();
        literals.push(literal.to_string());
    }
    for caps in plain_re.captures_iter(source) {
        let literal = caps[1]
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\r", "\r")
            .replace("\\\"", "\"")
            .replace("\\'", "'")
            .replace("\\\\", "\\");
        literals.push(literal);
    }
    literals.retain(|literal| tag_re.is_match(literal));
    literals
}

/// Every page of the corpus, each once.
fn corpus() -> Vec<String> {
    fn add(pages: &mut Vec<String>, seen: &mut BTreeSet<String>, page: String) {
        if seen.insert(page.clone()) {
            pages.push(page);
        }
    }
    let mut seen = BTreeSet::new();
    let mut pages = Vec::new();

    for page in handwritten_pages() {
        add(&mut pages, &mut seen, page);
    }
    let handwritten = pages.len();

    let mut rng = Rng(0x5C4A_9012_2026_0090);
    for _ in 0..800 {
        add(&mut pages, &mut seen, random_page(&mut rng));
    }
    let random = pages.len() - handwritten;

    let sources = workspace_sources();
    for path in &sources {
        if let Ok(source) = std::fs::read_to_string(path) {
            for literal in html_literals(&source) {
                add(&mut pages, &mut seen, literal);
            }
        }
    }
    let harvested = pages.len() - handwritten - random;

    assert!(handwritten >= 100, "{handwritten} handwritten pages");
    assert!(random >= 700, "{random} random pages");
    if !sources.is_empty() {
        assert!(
            harvested >= 100,
            "{harvested} HTML literals in the workspace"
        );
    }
    eprintln!(
        "differential corpus: {} pages ({handwritten} handwritten, {random} random, \
         {harvested} literals from {} source files)",
        pages.len(),
        sources.len()
    );
    pages
}

/// Page URLs the conversions are resolved against, one of them not a URL.
const PAGE_URLS: &[&str] = &[
    "https://example.com/docs/guide/page",
    "https://example.com",
    "http://localhost:8080/a/b/?q=1#frag",
    "https://exämple.org/päth/",
    "not a url",
];

/// Markdown written by hand for the stages that take Markdown.
fn markdown_samples() -> Vec<String> {
    let mut samples: Vec<String> = [
        "",
        "plain",
        "[a](/b)",
        "[a](b) [c](https://x.example/d) [e](#f) [g](mailto:h@i) [j](//k/l) [m](?n) [o]()",
        "![a](/b.png) ![](c.png) ![d]() [![e](f.png)](/g) [![](h.png)](/i) [![j]()]()",
        "[a](/b \"title\") [c]( /d ) [e](/f\t\"t\" ) [g](/h \"unclosed) [i](/j \"a\" \"b\")",
        "```\n[a](/b)\n```\n[c](/d)\n~~~\n[e](/f)\n~~~\nsee `[g](/h)` and [i](/j)",
        "```\n[a](/b)\n~~~\n[c](/d)\n```\n[e](/f)\n   ```rust\n[g](/h)",
        "`[a](/b) [c](/d)` [e](/f) ` [g](/h)",
        "[a]\n(/b) [c](\n/d) [\n\ne\n\n](/f) ![\n g \n](/h) [ \n i \n ](j)",
        "[\n\n\n](x) [\n](y) [ ](z) [\u{a0}\nq\n](w)",
        "[](a) [](b)[](c) ![](d) ![x](/_next/y) ![x](/_nextz) [x](/_next/y) ![[]](/_next/)",
        "[a [b](c)](d) [a](b(c)d) [a]](b) [[a]](b) [a](b)) \\[a\\](b) [a\\](b)",
        "[a](x:y) [a](1:2) [a](c:\\d) [a](:e) [a](a/b:c) [a](HTTP://U/) [a](ht tp://x) [a](%3A)",
        "[a](http://[::1]/x) [a](http://[bad) [a](file:///etc) [a](data:,x) [a](javascript:1)",
        "[a]( \"t t\" ) [b](  \"t\") [c](x \"t\" y) [d](x \"t\"\"u\") [e](\"t\" ) [f]( \"t) [g](x \"a)b\" )",
        "[a](x\u{a0}\"t\"\u{2003}) [b](\u{a0}y\u{a0}) [c](\t\"q r\"\t) [d](x \"t\" ) tail) [e](x  \"\")",
        "[![a](b c)](d) [![a](b)![c](d e)](f) [x ![a ![b](c)](d) [![a](b c)![d](e f)](g h)",
        "![a ![b](c d)](e) ![![![a](b c)](d)](e) [![a](b c)](d \"t\") [!](x) [![](y) [![]](z)",
        "!![a](b) ![a](b)![c](d) [a](b)![c](d) !\u{e9}[a](b) [a](b)[c](d)!",
        "[a](MAILTO:Me@Example.ORG) [a](http:rel) [a](https:rel) [a](HTTP:/abs) [a](https:/abs)",
        "[a](http:) [a](https://) [a](x:%20%zz) [a](WS://h/p/../q) [a](http://h/a/./b/../c)",
        "[a](http://EXAMPLE.org:80/%7Euser) [a](https://h:443) [a](ftp://u:p@h/) [a](a:b:c)",
        "<img src=\"a.png\" alt=\"A\"> <IMG ALT=\"B\"> <img src=\"c.png\"> <img> <img > <img\nsrc=\"d\">",
        "<img src='single.png' alt='single'> <img data-src=\"e.png\" alt = \"spaced\" src= \"f\">",
        "<b>bold</b> <br/> <a href=\"x\">y</a> < not a tag > <> <<x>> 1 < 2 > 1 a<b",
        "\\<escaped\\> \\<img src=\"x\" alt=\"y\"\\> <sub>s</sub> <details>\n<summary>S</summary>\n</details>",
        "#no space\n##two\n### three\n#\n##\n# \n#  spaced\n####### seven\n #indented\n\t#tab",
        "a  b   c\n  lead  \n\n\n\nd\n \n \ne   \n```\n  code  x\n\n\n```\nf  g\n```\nunclosed  h",
        "    ```\nnot  a  fence start\n```\nfence  now\n```\nout  again",
        "line\r\nwith  crlf\r\n\r\n\r\nend\r",
        "tab\tand  \t  mixed\u{a0}\u{a0}nbsp  \u{2003}  em",
        "trailing newline\n",
        "\n\n\nleading newlines",
        "   ",
        "\n",
        "#",
        "# ",
        "#\u{a0}x",
        "#é",
    ]
    .iter()
    .map(|sample| sample.to_string())
    .collect();

    // Random Markdown from link and noise pieces.
    const PIECES: &[&str] = &[
        "[",
        "]",
        "(",
        ")",
        "](",
        "![",
        "[](",
        "![](",
        "![a](b)",
        "![a](b c)",
        "](x)",
        "]( \"t\")",
        " \"t\"",
        "\"",
        ")](",
        "[a](b)",
        "\n",
        "\n\n",
        " ",
        "  ",
        "`",
        "```",
        "~~~",
        "#",
        "<",
        ">",
        "<img ",
        "alt=\"x\"",
        "src=\"y\"",
        "/_next/",
        "/a",
        "b",
        "c:d",
        "\"t\"",
        "text",
        "\\",
        "!",
        "\t",
        "\u{a0}",
        "é",
        "https://x.example/",
        "#frag",
    ];
    let mut rng = Rng(0x00C0_FFEE_1234_5678);
    for _ in 0..1500 {
        let mut sample = String::new();
        for _ in 0..(1 + rng.below(30)) {
            sample.push_str(rng.pick(PIECES));
        }
        samples.push(sample);
    }
    samples
}

// =============================================================================
// Tests
// =============================================================================

/// A short, printable name for a failing page.
fn excerpt(page: &str) -> String {
    let end = page
        .char_indices()
        .nth(300)
        .map_or(page.len(), |(index, _)| index);
    format!("{:?} ({} bytes)", &page[..end], page.len())
}

#[test]
fn public_entry_points_match_the_legacy_implementation() {
    let pages = corpus();
    let configs = [
        MarkdownConfig::default(),
        MarkdownConfig {
            only_main_content: true,
            ..MarkdownConfig::default()
        },
        MarkdownConfig {
            max_line_length: Some(80),
            preserve_link_titles: false,
            convert_images: false,
            convert_code_blocks: false,
            only_main_content: false,
        },
        MarkdownConfig {
            max_line_length: Some(1),
            preserve_link_titles: false,
            convert_images: false,
            convert_code_blocks: false,
            only_main_content: true,
        },
    ];

    let mut comparisons = 0usize;
    for (index, page) in pages.iter().enumerate() {
        let full = html_to_markdown(page);
        assert_eq!(
            full,
            legacy::html_to_markdown(page),
            "html_to_markdown: {}",
            excerpt(page)
        );
        let main = html_to_main_content_markdown(page);
        assert_eq!(
            main,
            legacy::html_to_main_content_markdown(page),
            "html_to_main_content_markdown: {}",
            excerpt(page)
        );
        for config in &configs {
            assert_eq!(
                html_to_markdown_with_config(page, config),
                legacy::html_to_markdown_with_config(page, config),
                "html_to_markdown_with_config({config:?}): {}",
                excerpt(page)
            );
        }
        comparisons += 2 + configs.len();

        // The page URL only matters to the last stage: every URL on a part
        // of the corpus, a rotating one on the rest.
        let urls: &[&str] = if index % 8 == 0 {
            PAGE_URLS
        } else {
            std::slice::from_ref(&PAGE_URLS[index % PAGE_URLS.len()])
        };
        for page_url in urls {
            assert_eq!(
                document_base_url(page, page_url),
                legacy::document_base_url(page, page_url),
                "document_base_url({page_url}): {}",
                excerpt(page)
            );
            for only_main_content in [false, true] {
                assert_eq!(
                    html_to_markdown_for_page(page, page_url, only_main_content),
                    legacy::html_to_markdown_for_page(page, page_url, only_main_content),
                    "html_to_markdown_for_page({page_url}, {only_main_content}): {}",
                    excerpt(page)
                );
                comparisons += 1;
            }
            // What `/scrape` does: the Markdown, then its links.
            if let Some(base) = document_base_url(page, page_url) {
                for markdown in [&full, &main] {
                    assert_eq!(
                        absolutize_markdown_links(markdown, &base),
                        legacy::absolutize_markdown_links(markdown, &base),
                        "absolutize_markdown_links({base}): {}",
                        excerpt(page)
                    );
                    comparisons += 1;
                }
            }
        }
    }
    eprintln!("{comparisons} conversions compared");
}

#[test]
fn stages_match_the_legacy_implementation() {
    // Each stage gets the corpus pages and the Markdown samples, whatever
    // it normally takes: HTML is hard Markdown and the reverse.
    let mut inputs = corpus();
    inputs.extend(markdown_samples());
    let bases: Vec<url::Url> = [
        "https://x.example/",
        "https://x.example/docs/page?q=1#f",
        "http://localhost:8080/a/b/",
        "file:///tmp/dir/page.html",
    ]
    .iter()
    .map(|base| url::Url::parse(base).unwrap())
    .collect();

    for input in &inputs {
        let stripped = strip_non_content_tags(input);
        let legacy_stripped = legacy::strip_non_content_tags(input);
        assert_eq!(
            stripped,
            legacy_stripped,
            "strip_non_content_tags: {}",
            excerpt(input)
        );
        // Borrowed exactly when the legacy one is: nothing copied for nothing.
        assert_eq!(
            matches!(stripped, Cow::Borrowed(_)),
            matches!(legacy_stripped, Cow::Borrowed(_)),
            "strip_non_content_tags copies: {}",
            excerpt(input)
        );
        assert_eq!(
            strip_boilerplate_tags(stripped),
            legacy::strip_boilerplate_tags(legacy_stripped),
            "strip_boilerplate_tags after the non-content tags: {}",
            excerpt(input)
        );
        // What the conversion calls: the nine tags from one scan.
        assert_eq!(
            strip_tags(Cow::Borrowed(input), 0..STRIPPED_TAGS.len()),
            legacy::strip_boilerplate_tags(legacy::strip_non_content_tags(input)),
            "strip_tags, all nine: {}",
            excerpt(input)
        );
        assert_eq!(
            strip_boilerplate_tags(Cow::Borrowed(input)),
            legacy::strip_boilerplate_tags(Cow::Borrowed(input)),
            "strip_boilerplate_tags: {}",
            excerpt(input)
        );
        assert_eq!(
            strip_remaining_html(input),
            legacy::strip_remaining_html(input),
            "strip_remaining_html: {}",
            excerpt(input)
        );
        assert_eq!(
            clean_markdown(input),
            legacy::clean_markdown(input),
            "clean_markdown: {}",
            excerpt(input)
        );
        for base in &bases {
            assert_eq!(
                absolutize_markdown_links(input, base),
                legacy::absolutize_markdown_links(input, base),
                "absolutize_markdown_links({base}): {}",
                excerpt(input)
            );
        }
    }
}

/// Random lines from the pieces of a link, most of them plain (no `"`,
/// no `![`): the lines whose links are found without the regex.
fn link_lines() -> Vec<String> {
    const PLAIN: &[&str] = &[
        "[",
        "[",
        "]",
        "]",
        "(",
        ")",
        ")",
        "](",
        "](",
        "](",
        " ",
        "  ",
        "\t",
        "\u{a0}",
        "\u{2003}",
        "a",
        "b c",
        "/p",
        "x:y",
        "#f",
        "https://h.example/q",
        "é",
        "!",
        "! ",
        "[a](b)",
        "[a]( b )",
        "[](c)",
        "[d]()",
        "](e)",
        "[[",
        "]]",
        "((",
        "))",
        "\\",
        "<",
        ">",
        "//cdn/x",
        "?q=1",
        "..",
    ];
    const OTHER: &[&str] = &["![", "![a](b)", "\"", " \"t\"", "![](", "\"t\")"];
    let mut rng = Rng(0x1EAF_0000_BEEF_0042);
    let mut lines = Vec::new();
    for round in 0..6_000 {
        let mut line = String::new();
        for _ in 0..(1 + rng.below(24)) {
            // One line in three also draws from the pieces that make a
            // line not plain.
            if round % 3 == 0 && rng.below(6) == 0 {
                line.push_str(rng.pick(OTHER));
            } else {
                line.push_str(rng.pick(PLAIN));
            }
        }
        lines.push(line);
    }
    lines
}

#[test]
fn links_are_rewritten_as_before() {
    let base = url::Url::parse("https://x.example/docs/page?q=1#f").unwrap();
    let mut lines = link_lines();
    for input in corpus().iter().chain(markdown_samples().iter()) {
        lines.push(input.clone());
        lines.extend(input.lines().map(str::to_string));
        // The Markdown of the page, which is what the stage sees.
        lines.extend(html_to_markdown(input).lines().map(str::to_string));
    }
    lines.retain(|line| line.contains("]("));
    assert!(lines.len() > 5_000, "{} lines with a link", lines.len());

    let mut plain = 0usize;
    for line in &lines {
        let expected = legacy::rewrite_links(line, &base);
        assert_eq!(
            rewrite_links_with_regex(line, &base),
            expected,
            "regex: {}",
            excerpt(line)
        );
        let mut rewritten = String::from("kept ");
        push_rewritten_links(&mut rewritten, line, &base);
        assert_eq!(
            rewritten.strip_prefix("kept "),
            Some(expected.as_str()),
            "{}",
            excerpt(line)
        );
        if !line.contains('"') && !line.contains("![") {
            plain += 1;
        }
    }
    assert!(plain > 3_000, "{plain} plain lines");
    eprintln!("{} lines with a link, {plain} of them plain", lines.len());
}

/// The regex engine does not return the leftmost match on these lines: a
/// matcher written from the pattern would, and change the Markdown. They
/// are why only plain lines skip the regex. If this test fails after a
/// `regex` upgrade, the engine changed what it matches: the Markdown of
/// such lines changes with it, whatever this module does.
#[test]
fn the_link_regex_is_not_leftmost_on_lines_that_are_not_plain() {
    let anchored = Regex::new(&format!("^(?:{})", link_regex().as_str())).unwrap();
    for (line, found) in [
        // An image whose source holds a link.
        ("[![](a [b](c)d](e)", "[b](c)"),
        // A title that holds a link.
        ("[a](x \"t [b](c) u\")", "[b](c)"),
    ] {
        assert_eq!(anchored.find(line).map(|m| m.as_str()), Some(line));
        assert_eq!(link_regex().find(line).map(|m| m.as_str()), Some(found));
    }
}

/// Plain lines are scanned once, whatever they hold.
#[test]
fn hostile_plain_link_lines_stay_linear() {
    let base = url::Url::parse("https://x.example/").unwrap();
    let lines = [
        "[".repeat(300_000) + "](x)",
        "[".repeat(300_000) + "] (x)",
        "[a](".repeat(100_000) + " b",
        "[a](x ".repeat(100_000),
        "[a]( ".to_string() + &" ".repeat(300_000) + "b c)",
        "](".repeat(150_000) + " )",
        "[a]".repeat(100_000) + "(b)",
    ];
    let start = std::time::Instant::now();
    for line in &lines {
        let mut rewritten = String::new();
        push_rewritten_plain_links(&mut rewritten, line, &base);
        assert_eq!(
            rewritten,
            legacy::rewrite_links(line, &base),
            "{}",
            excerpt(line)
        );
    }
    // Well under a second in a debug build; minutes if a line is rescanned
    // from every `[`.
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
}

#[test]
fn every_line_is_cleaned_as_before() {
    let mut lines: Vec<String> = Vec::new();
    for input in corpus().iter().chain(markdown_samples().iter()) {
        lines.extend(input.lines().map(|line| line.trim().to_string()));
    }
    lines.retain(|line| !line.is_empty());
    for line in &lines {
        let mut cleaned = String::new();
        push_clean_line(&mut cleaned, line);
        assert_eq!(cleaned, legacy::clean_line(line), "{}", excerpt(line));
    }
}

/// The scanner compares tag names as ASCII and takes `char::is_whitespace`
/// for the regex's `\s`. Both hold only as long as the regex engine folds
/// no character but `ſ` (and `K`, which no stripped tag has) to an ASCII
/// letter, and agrees with `char` on whitespace: checked on every character.
#[test]
fn scanner_assumptions_hold_for_every_character() {
    let letter = Regex::new(r"(?i)^[a-z]$").unwrap();
    let space = Regex::new(r"(?s)^\s$").unwrap();
    let mut buffer = [0u8; 4];
    for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
        let text: &str = c.encode_utf8(&mut buffer);
        let folds_to_ascii_letter = c.is_ascii_alphabetic() || c == '\u{17f}' || c == '\u{212a}';
        assert_eq!(letter.is_match(text), folds_to_ascii_letter, "{c:?}");
        assert_eq!(space.is_match(text), c.is_whitespace(), "{c:?}");
    }
    for tag in STRIPPED_TAGS {
        assert!(!tag.contains('k'), "{tag}: `K` (U+212A) folds to k");
        assert!(tag.bytes().all(|b| b.is_ascii_lowercase()), "{tag}");
        assert!(
            matches!(tag.as_bytes()[0], b's' | b'n' | b'h' | b'f' | b'a'),
            "{tag}: tag_mark filters on the first letter"
        );
        assert!(
            tag.len() < TAG_MARK_REACH,
            "{tag}: `</{tag}` is read past the reach"
        );
    }
}

#[test]
fn every_byte_is_found_once_and_in_order() {
    let mut rng = Rng(0x0BAD_5EED_0000_0001);
    for len in 0..70 {
        for _ in 0..40 {
            // Bytes around `<`, with the values that fool a careless SWAR
            // test (0x3d, 0x3b, 0xbc, 0x00, 0x80) well represented.
            let bytes: Vec<u8> = (0..len)
                .map(|_| [b'<', b'<', 0x3d, 0x3b, 0xbc, 0x00, 0x80, b'a', 0xff][rng.below(9)])
                .collect();
            let mut found = Vec::new();
            for_each_byte(&bytes, b'<', |at| found.push(at));
            let expected: Vec<usize> = (0..len).filter(|&at| bytes[at] == b'<').collect();
            assert_eq!(found, expected, "{bytes:?}");
        }
    }
}

#[test]
fn a_long_s_page_goes_through_the_regexes() {
    // `(?i)` folds `ſ` to `s`: the legacy regexes strip these blocks.
    let page = "<\u{17f}cript>gone</\u{17f}cript><a\u{17f}ide>gone too</ASIDE><p>kept</p>";
    assert_eq!(
        strip_non_content_tags(page),
        "<a\u{17f}ide>gone too</ASIDE><p>kept</p>"
    );
    assert_eq!(
        strip_boilerplate_tags(strip_non_content_tags(page)),
        "<p>kept</p>"
    );
}

/// Where the time goes, stage by stage, before and after. Not a test:
/// `cargo test --release -p scrapix-parser profile_markdown_stages -- --ignored --nocapture`
#[test]
#[ignore = "profiling helper, run it in release mode with --nocapture"]
fn profile_markdown_stages() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    fn time<T>(mut f: impl FnMut() -> T) -> Duration {
        // Best of several batches: other processes only ever add time.
        let mut best = Duration::MAX;
        for _ in 0..15 {
            let mut runs = 0u32;
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(40) {
                black_box(f());
                runs += 1;
            }
            best = best.min(start.elapsed() / runs);
        }
        best
    }

    let medium = html_literals(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/parser_benchmarks.rs"),
        )
        .unwrap(),
    )
    .into_iter()
    .find(|literal| literal.contains("Getting Started"))
    .unwrap();
    let pages = [
        ("medium", medium),
        ("large_100p", bench_large_html(100, 500)),
        ("realistic_60s", bench_realistic_html(60)),
    ];
    let page_url = "https://example.com/docs/guide/page";

    for (name, page) in &pages {
        let base = document_base_url(page, page_url).unwrap();
        let stripped = legacy::strip_boilerplate_tags(legacy::strip_non_content_tags(page));
        let converted = html2md::parse_html(&stripped);
        let remaining = legacy::strip_remaining_html(&converted);
        let cleaned = legacy::clean_markdown(&remaining);

        let micros = |d: Duration| d.as_secs_f64() * 1e6;
        let rows = [
            (
                "strip tags (9 passes)",
                time(|| legacy::strip_boilerplate_tags(legacy::strip_non_content_tags(page)).len()),
                time(|| strip_tags(Cow::Borrowed(page), 0..STRIPPED_TAGS.len()).len()),
            ),
            (
                "html2md::parse_html",
                time(|| html2md::parse_html(&stripped)),
                time(|| html2md::parse_html(&stripped)),
            ),
            (
                "strip remaining html",
                time(|| legacy::strip_remaining_html(&converted).len()),
                time(|| strip_remaining_html(&converted).len()),
            ),
            (
                "clean markdown",
                time(|| legacy::clean_markdown(&remaining)),
                time(|| clean_markdown(&remaining)),
            ),
            (
                "base url + absolutize",
                time(|| {
                    let base = legacy::document_base_url(page, page_url).unwrap();
                    legacy::absolutize_markdown_links(&cleaned, &base)
                }),
                time(|| {
                    let base = document_base_url(page, page_url).unwrap();
                    absolutize_markdown_links(&cleaned, &base)
                }),
            ),
            (
                "TOTAL html_to_markdown_for_page(main)",
                time(|| legacy::html_to_markdown_for_page(page, page_url, true)),
                time(|| html_to_markdown_for_page(page, page_url, true)),
            ),
        ];
        black_box(&base);
        let (total_before, total_after) = (micros(rows[5].1), micros(rows[5].2));
        println!(
            "\n{name}: {} bytes of HTML, {} bytes of Markdown",
            page.len(),
            cleaned.len()
        );
        let links = cleaned.matches("](").count();
        let join_rel = time(|| base.join(black_box("guide/topic-12")));
        let parse_abs = time(|| url::Url::parse(black_box("https://example.org/ref/12")));
        println!(
            "{links} links; one base.join(relative) {:.0} ns, one Url::parse(absolute) {:.0} ns",
            join_rel.as_secs_f64() * 1e9,
            parse_abs.as_secs_f64() * 1e9
        );
        // What html2md cannot go under: parsing the same HTML into a DOM
        // (scraper's, the same html5ever underneath).
        let dom = time(|| scraper::Html::parse_document(&stripped));
        println!(
            "parsing the stripped HTML into a DOM alone: {:.1} µs",
            micros(dom)
        );
        println!(
            "{:<40} {:>10} {:>7} {:>10} {:>7}",
            "stage", "before µs", "share", "after µs", "share"
        );
        for (stage, before, after) in rows {
            println!(
                "{:<40} {:>10.1} {:>6.1}% {:>10.1} {:>6.1}%",
                stage,
                micros(before),
                100.0 * micros(before) / total_before,
                micros(after),
                100.0 * micros(after) / total_after
            );
        }
    }
}
