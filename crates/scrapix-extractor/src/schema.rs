//! Schema.org and JSON-LD extraction from HTML documents
//!
//! Extracts structured data in JSON-LD and Microdata formats.

use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use tracing::{debug, instrument, warn};

/// Errors that can occur during schema extraction
#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("Invalid JSON-LD: {0}")]
    InvalidJsonLd(String),

    #[error("Invalid selector: {0}")]
    InvalidSelector(String),

    #[error("Extraction error: {0}")]
    ExtractionError(String),
}

/// Configuration for schema extraction
#[derive(Debug, Clone, Default)]
pub struct SchemaConfig {
    /// Only extract these schema types (empty = all types)
    pub only_types: HashSet<String>,

    /// Convert ISO date strings to timestamps
    pub convert_dates: bool,

    /// Flatten nested @graph structures
    pub flatten_graph: bool,

    /// Include raw JSON-LD scripts in output
    pub include_raw: bool,
}

/// Extracted schema data from an HTML document
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ExtractedSchema {
    /// All JSON-LD data found (may be array or object)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Value)]
    pub json_ld: Option<Value>,

    /// Individual schema items by type
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<SchemaItem>,

    /// Microdata items
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub microdata: Vec<SchemaItem>,

    /// Number of JSON-LD scripts found
    pub json_ld_count: usize,

    /// Number of microdata items found
    pub microdata_count: usize,
}

/// A single schema.org item
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct SchemaItem {
    /// Schema type (e.g., "Article", "Product", "Organization")
    #[serde(rename = "@type")]
    pub schema_type: String,

    /// Full schema data
    #[serde(flatten)]
    #[schema(value_type = Object)]
    pub data: Value,
}

impl SchemaItem {
    /// An item of `schema_type` (one entry when the source `@type` is an
    /// array). `data` stays the source object, `@type` included: the crawler
    /// indexes it as is.
    pub fn new(schema_type: String, data: Value) -> Self {
        Self { schema_type, data }
    }
}

/// `@type` (the item's `schema_type`) then `data`'s other keys: a derived
/// `#[serde(flatten)]` wrote `@type` twice, once from each.
impl Serialize for SchemaItem {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("@type", &self.schema_type)?;
        if let Value::Object(data) = &self.data {
            for (key, value) in data.iter().filter(|(key, _)| *key != "@type") {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// Schema extractor
pub struct SchemaExtractor {
    config: SchemaConfig,
    jsonld_selector: Selector,
    microdata_selector: Selector,
    id_selector: Selector,
}

impl Default for SchemaExtractor {
    fn default() -> Self {
        Self::new(SchemaConfig::default())
    }
}

impl SchemaExtractor {
    /// Create a new schema extractor with configuration
    pub fn new(config: SchemaConfig) -> Self {
        Self {
            config,
            jsonld_selector: Selector::parse("script[type='application/ld+json']")
                .expect("valid jsonld selector"),
            microdata_selector: Selector::parse("[itemscope]").expect("valid microdata selector"),
            id_selector: Selector::parse("[id]").expect("valid id selector"),
        }
    }

    /// Create extractor with default config
    pub fn with_defaults() -> Self {
        Self::default()
    }

    /// Create extractor that only extracts specific types
    pub fn with_types(types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new(SchemaConfig {
            only_types: types.into_iter().map(|t| t.into()).collect(),
            ..Default::default()
        })
    }

    /// Extract all schema data from an HTML document
    #[instrument(skip(self, html), level = "debug")]
    pub fn extract(&self, html: &str) -> Result<ExtractedSchema, SchemaError> {
        let document = Html::parse_document(html);
        self.extract_from_dom(&document)
    }

    /// Extract all schema data from a pre-parsed DOM, avoiding redundant parsing
    pub fn extract_from_dom(&self, document: &Html) -> Result<ExtractedSchema, SchemaError> {
        let mut result = ExtractedSchema::default();

        // Extract JSON-LD
        self.extract_json_ld(document, &mut result)?;

        // Extract Microdata
        self.extract_microdata(document, &mut result);

        debug!(
            json_ld_count = result.json_ld_count,
            microdata_count = result.microdata_count,
            items_count = result.items.len(),
            "Extracted schema data"
        );

        Ok(result)
    }

    /// Extract JSON-LD scripts
    fn extract_json_ld(
        &self,
        document: &Html,
        result: &mut ExtractedSchema,
    ) -> Result<(), SchemaError> {
        let mut all_json_ld: Vec<Value> = Vec::new();

        for script in document.select(&self.jsonld_selector) {
            let content = script.text().collect::<String>();
            let content = content.trim();

            if content.is_empty() {
                continue;
            }

            // Parse JSON
            match serde_json::from_str::<Value>(content) {
                Ok(json) => {
                    result.json_ld_count += 1;

                    // Process the JSON-LD
                    self.process_json_ld(&json, result);

                    // Store raw JSON
                    all_json_ld.push(json);
                }
                Err(e) => {
                    warn!("Failed to parse JSON-LD: {}", e);
                    // Continue processing other scripts
                }
            }
        }

        // Combine all JSON-LD into result
        if !all_json_ld.is_empty() {
            result.json_ld = if all_json_ld.len() == 1 {
                Some(all_json_ld.into_iter().next().unwrap())
            } else {
                Some(Value::Array(all_json_ld))
            };
        }

        Ok(())
    }

    /// Process a JSON-LD value and extract schema items
    fn process_json_ld(&self, json: &Value, result: &mut ExtractedSchema) {
        match json {
            Value::Array(arr) => {
                for item in arr {
                    self.process_json_ld(item, result);
                }
            }
            Value::Object(obj) => {
                // Check for @graph (multiple items in one script)
                if let Some(graph) = obj.get("@graph") {
                    self.process_json_ld(graph, result);
                    return;
                }

                // Extract @type
                if let Some(type_value) = obj.get("@type") {
                    let types = self.extract_types(type_value);

                    for schema_type in types {
                        // Filter by configured types if any
                        if !self.config.only_types.is_empty()
                            && !self.config.only_types.contains(&schema_type)
                        {
                            continue;
                        }

                        let mut data = json.clone();

                        // Convert dates if configured
                        if self.config.convert_dates {
                            self.convert_dates_in_value(&mut data);
                        }

                        result.items.push(SchemaItem::new(schema_type, data));
                    }
                }
            }
            _ => {}
        }
    }

    /// Extract type(s) from @type value (can be string or array)
    fn extract_types(&self, type_value: &Value) -> Vec<String> {
        match type_value {
            Value::String(s) => vec![normalize_type(s)],
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(normalize_type)
                .collect(),
            _ => vec![],
        }
    }

    /// Convert ISO date strings to timestamps in a JSON value
    fn convert_dates_in_value(&self, value: &mut Value) {
        match value {
            Value::Object(obj) => {
                for (key, val) in obj.iter_mut() {
                    // Check if this looks like a date field
                    let is_date_field = key.to_lowercase().contains("date")
                        || key.to_lowercase().contains("time")
                        || key == "datePublished"
                        || key == "dateModified"
                        || key == "dateCreated"
                        || key == "startDate"
                        || key == "endDate";

                    if is_date_field {
                        if let Value::String(s) = val {
                            // Try to parse ISO date and convert to timestamp
                            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                                *val = Value::Number(dt.timestamp().into());
                            } else if let Ok(dt) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                            {
                                let datetime = dt.and_hms_opt(0, 0, 0).unwrap().and_utc();
                                *val = Value::Number(datetime.timestamp().into());
                            }
                        }
                    } else {
                        self.convert_dates_in_value(val);
                    }
                }
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    self.convert_dates_in_value(item);
                }
            }
            _ => {}
        }
    }

    /// Extract Microdata items
    fn extract_microdata(&self, document: &Html, result: &mut ExtractedSchema) {
        result.microdata = self.microdata_from_dom(document);
        result.microdata_count = result.microdata.len();
    }

    /// The page's top-level Microdata items that name a type, with the
    /// configured type filter and date conversion applied.
    ///
    /// An item is top-level when it is not a property of another item: it
    /// has no `itemprop`, or no item around it. An `Article` inside a
    /// `<body itemscope itemtype="…/WebPage">` is one.
    pub fn microdata_from_dom(&self, document: &Html) -> Vec<SchemaItem> {
        let mut walk = MicrodataWalk {
            document,
            id_selector: &self.id_selector,
            ids: None,
            budget: MICRODATA_NODE_BUDGET,
            open: Vec::new(),
        };
        let mut items = Vec::new();

        for element in document.select(&self.microdata_selector) {
            if walk.budget == 0 {
                debug!("Microdata extraction stopped: page too large or too nested");
                break;
            }
            if element.value().attr("itemprop").is_some() && walk.has_item_ancestor(&element) {
                continue;
            }
            // An item without a type names no vocabulary: not schema.org data.
            let Some(schema_type) = item_type(&element) else {
                continue;
            };
            if !self.config.only_types.is_empty() && !self.config.only_types.contains(&schema_type)
            {
                continue;
            }

            let mut data = walk.read_item(element, 0);
            if self.config.convert_dates {
                self.convert_dates_in_value(&mut data);
            }
            items.push(SchemaItem::new(schema_type, data));
        }

        items
    }

    /// Get a specific schema type from extracted data
    pub fn get_by_type<'a>(
        &self,
        result: &'a ExtractedSchema,
        schema_type: &str,
    ) -> Vec<&'a SchemaItem> {
        result
            .items
            .iter()
            .chain(result.microdata.iter())
            .filter(|item| item.schema_type == schema_type)
            .collect()
    }

    /// Extract a specific property from schema items of a given type
    pub fn get_property(
        &self,
        result: &ExtractedSchema,
        schema_type: &str,
        property: &str,
    ) -> Option<Value> {
        for item in self.get_by_type(result, schema_type) {
            if let Value::Object(obj) = &item.data {
                if let Some(value) = obj.get(property) {
                    return Some(value.clone());
                }
            }
        }
        None
    }
}

/// Deepest nesting of Microdata items read (an item as the value of another
/// item's property); deeper ones are dropped.
const MICRODATA_MAX_DEPTH: usize = 16;

/// Nodes one page's Microdata extraction may visit. A page is walked about
/// once; `itemref` and nested text values can repeat parts of it, and this
/// bounds the work on a page built to repeat them.
const MICRODATA_NODE_BUDGET: usize = 500_000;

/// The type of a Microdata item: the first `itemtype`, without the
/// schema.org prefix. `None` for an item without a type.
fn item_type(element: &ElementRef) -> Option<String> {
    element
        .value()
        .attr("itemtype")?
        .split_whitespace()
        .next()
        .map(normalize_type)
}

/// Normalize a schema type (remove the schema.org prefix)
fn normalize_type(type_str: &str) -> String {
    type_str
        .trim()
        .replace("https://schema.org/", "")
        .replace("http://schema.org/", "")
}

/// One page's Microdata extraction.
struct MicrodataWalk<'a> {
    document: &'a Html,
    id_selector: &'a Selector,
    /// Elements by `id`, built the first time an `itemref` is met.
    ids: Option<HashMap<&'a str, ElementRef<'a>>>,
    /// Nodes left to visit (see [`MICRODATA_NODE_BUDGET`]).
    budget: usize,
    /// The items being read, outermost first: an `itemref` loop stops at
    /// an item that is already open.
    open: Vec<ElementRef<'a>>,
}

impl<'a> MicrodataWalk<'a> {
    /// Spend one node of the budget; `false` once it is used up.
    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        true
    }

    /// Whether `element` sits inside another item.
    fn has_item_ancestor(&mut self, element: &ElementRef<'a>) -> bool {
        for ancestor in element.ancestors() {
            if !self.spend() {
                return false;
            }
            if ancestor
                .value()
                .as_element()
                .is_some_and(|e| e.attr("itemscope").is_some())
            {
                return true;
            }
        }
        false
    }

    /// The element with this `id` (the first one in the page).
    fn element_by_id(&mut self, id: &str) -> Option<ElementRef<'a>> {
        let (document, selector) = (self.document, self.id_selector);
        self.ids
            .get_or_insert_with(|| {
                let mut ids = HashMap::new();
                for element in document.select(selector) {
                    if let Some(id) = element.value().attr("id") {
                        ids.entry(id).or_insert(element);
                    }
                }
                ids
            })
            .get(id)
            .copied()
    }

    /// An item as a JSON object: `@type`, `@id` (`itemid`) and its
    /// properties. A property set several times is an array.
    ///
    /// The properties are the `itemprop` elements under the item and under
    /// the elements its `itemref` names, without entering another item.
    fn read_item(&mut self, root: ElementRef<'a>, depth: usize) -> Value {
        let mut props: serde_json::Map<String, Value> = serde_json::Map::new();
        if let Some(schema_type) = item_type(&root) {
            props.insert("@type".to_string(), Value::String(schema_type));
        }
        if let Some(id) = root.value().attr("itemid").map(str::trim) {
            if !id.is_empty() {
                props.insert("@id".to_string(), Value::String(id.to_string()));
            }
        }

        // Elements still to look at, the next one last: the item's children
        // in document order, then the elements `itemref` names.
        let mut pending: Vec<ElementRef<'a>> = Vec::new();
        if let Some(itemref) = root.value().attr("itemref") {
            let referenced: Vec<_> = itemref
                .split_whitespace()
                .filter_map(|id| self.element_by_id(id))
                .collect();
            pending.extend(referenced.into_iter().rev());
        }
        pending.extend(root.children().rev().filter_map(ElementRef::wrap));

        self.open.push(root);
        while let Some(element) = pending.pop() {
            if !self.spend() {
                break;
            }
            if element.id() == root.id() {
                continue;
            }
            let is_item = element.value().attr("itemscope").is_some();

            if let Some(names) = element.value().attr("itemprop") {
                let value = if !is_item {
                    Some(self.property_value(&element))
                } else if depth + 1 < MICRODATA_MAX_DEPTH
                    && !self.open.iter().any(|open| open.id() == element.id())
                {
                    Some(self.read_item(element, depth + 1))
                } else {
                    None
                };
                if let Some(value) = value {
                    for name in names.split_whitespace() {
                        match props.get_mut(name) {
                            Some(Value::Array(values)) => values.push(value.clone()),
                            Some(existing) => {
                                *existing = Value::Array(vec![existing.take(), value.clone()]);
                            }
                            None => {
                                props.insert(name.to_string(), value.clone());
                            }
                        }
                    }
                }
            }

            // Another item's content belongs to that item.
            if !is_item {
                pending.extend(element.children().rev().filter_map(ElementRef::wrap));
            }
        }
        self.open.pop();

        Value::Object(props)
    }

    /// The value of a property that is not an item: its `content`
    /// attribute when it has one (also read outside `<meta>`, as search
    /// engines do), the element's own value attribute (`href`, `src`,
    /// `datetime`, …), else its text with whitespace collapsed.
    fn property_value(&mut self, element: &ElementRef<'a>) -> Value {
        let e = element.value();
        let attribute = e.attr("content").or_else(|| match e.name() {
            "link" | "a" | "area" => e.attr("href"),
            "img" | "audio" | "video" | "source" | "embed" | "iframe" | "track" => e.attr("src"),
            "object" => e.attr("data"),
            "time" => e.attr("datetime"),
            "data" | "meter" => e.attr("value"),
            _ => None,
        });
        if let Some(value) = attribute {
            return Value::String(value.trim().to_string());
        }

        let mut text = String::new();
        for chunk in element.text() {
            if !self.spend() {
                break;
            }
            for word in chunk.split_whitespace() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(word);
            }
        }
        Value::String(text)
    }
}

/// The `schema` field of a crawled document: its JSON-LD (`json_ld`, one
/// value or an array) followed by the page's Microdata items.
///
/// A Microdata item is left out when the JSON-LD already has a top-level
/// item of its type (`@graph` members count): pages that publish both
/// formats describe the same thing twice.
pub fn merge_microdata(json_ld: Option<Value>, microdata: Vec<SchemaItem>) -> Option<Value> {
    fn collect_types(value: &Value, types: &mut HashSet<String>) {
        match value {
            Value::Array(values) => values.iter().for_each(|v| collect_types(v, types)),
            Value::Object(object) => {
                if let Some(graph) = object.get("@graph") {
                    collect_types(graph, types);
                }
                match object.get("@type") {
                    Some(Value::String(t)) => {
                        types.insert(normalize_type(t));
                    }
                    Some(Value::Array(ts)) => {
                        types.extend(ts.iter().filter_map(Value::as_str).map(normalize_type));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let mut json_ld_types = HashSet::new();
    if let Some(json_ld) = &json_ld {
        collect_types(json_ld, &mut json_ld_types);
    }
    let mut added: Vec<Value> = microdata
        .into_iter()
        .filter(|item| !json_ld_types.contains(&item.schema_type))
        .map(|item| item.data)
        .collect();
    if added.is_empty() {
        return json_ld;
    }

    let mut values = match json_ld {
        None => Vec::new(),
        Some(Value::Array(values)) => values,
        Some(value) => vec![value],
    };
    values.append(&mut added);
    match values.len() {
        1 => values.pop(),
        _ => Some(Value::Array(values)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_ld_article() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <script type="application/ld+json">
                {
                    "@context": "https://schema.org",
                    "@type": "Article",
                    "headline": "Test Article",
                    "author": {
                        "@type": "Person",
                        "name": "John Doe"
                    },
                    "datePublished": "2024-01-15T10:00:00Z"
                }
                </script>
            </head>
            <body></body>
            </html>
        "#;

        let extractor = SchemaExtractor::default();
        let result = extractor.extract(html).unwrap();

        assert_eq!(result.json_ld_count, 1);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].schema_type, "Article");

        if let Value::Object(obj) = &result.items[0].data {
            assert_eq!(obj.get("headline").unwrap(), "Test Article");
            // The crawler indexes `data`: it keeps the source `@type`.
            assert_eq!(obj.get("@type").unwrap(), "Article");
        }

        // `@type` appears once in the serialized item (it used to be written
        // twice: the item's own and the flattened data's).
        let json = serde_json::to_string(&result.items[0]).unwrap();
        assert_eq!(
            json.matches("\"@type\"").count(),
            2,
            "item + author: {json}"
        );
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["@type"], "Article");
        assert_eq!(value["author"]["@type"], "Person");
    }

    #[test]
    fn test_extract_json_ld_graph() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <script type="application/ld+json">
                {
                    "@context": "https://schema.org",
                    "@graph": [
                        {
                            "@type": "Organization",
                            "name": "Test Org"
                        },
                        {
                            "@type": "WebSite",
                            "name": "Test Site"
                        }
                    ]
                }
                </script>
            </head>
            <body></body>
            </html>
        "#;

        let extractor = SchemaExtractor::default();
        let result = extractor.extract(html).unwrap();

        assert_eq!(result.items.len(), 2);
        assert!(result.items.iter().any(|i| i.schema_type == "Organization"));
        assert!(result.items.iter().any(|i| i.schema_type == "WebSite"));
    }

    #[test]
    fn test_extract_json_ld_product() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <script type="application/ld+json">
                {
                    "@context": "https://schema.org",
                    "@type": "Product",
                    "name": "Test Product",
                    "offers": {
                        "@type": "Offer",
                        "price": "99.99",
                        "priceCurrency": "USD"
                    }
                }
                </script>
            </head>
            <body></body>
            </html>
        "#;

        let extractor = SchemaExtractor::default();
        let result = extractor.extract(html).unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].schema_type, "Product");
    }

    #[test]
    fn test_filter_by_type() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <script type="application/ld+json">
                {
                    "@context": "https://schema.org",
                    "@graph": [
                        { "@type": "Article", "headline": "Article" },
                        { "@type": "Organization", "name": "Org" },
                        { "@type": "Person", "name": "Person" }
                    ]
                }
                </script>
            </head>
            <body></body>
            </html>
        "#;

        let extractor = SchemaExtractor::with_types(["Article", "Person"]);
        let result = extractor.extract(html).unwrap();

        assert_eq!(result.items.len(), 2);
        assert!(!result.items.iter().any(|i| i.schema_type == "Organization"));
    }

    #[test]
    fn test_extract_microdata() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <body>
                <div itemscope itemtype="https://schema.org/Product">
                    <span itemprop="name">Test Product</span>
                    <span itemprop="description">A great product</span>
                    <meta itemprop="sku" content="12345">
                </div>
            </body>
            </html>
        "#;

        let extractor = SchemaExtractor::default();
        let result = extractor.extract(html).unwrap();

        assert_eq!(result.microdata_count, 1);
        assert_eq!(result.microdata.len(), 1);
        assert_eq!(result.microdata[0].schema_type, "Product");

        if let Value::Object(obj) = &result.microdata[0].data {
            assert_eq!(obj.get("name").unwrap(), "Test Product");
            assert_eq!(obj.get("sku").unwrap(), "12345");
        }
    }

    fn microdata(html: &str) -> Vec<SchemaItem> {
        SchemaExtractor::default().extract(html).unwrap().microdata
    }

    /// A theme that marks `<body>` as a `WebPage` does not hide the items
    /// inside it: without `itemprop`, they are items of their own.
    #[test]
    fn microdata_item_inside_another_without_itemprop_is_top_level() {
        let items = microdata(
            r#"<body itemscope itemtype="https://schema.org/WebPage">
                <h1 itemprop="name">Page</h1>
                <article itemscope itemtype="https://schema.org/Article">
                    <h2 itemprop="headline">Post</h2>
                    <div itemprop="author" itemscope itemtype="https://schema.org/Person">
                        <span itemprop="name">Ada</span>
                    </div>
                </article>
            </body>"#,
        );
        let types: Vec<_> = items.iter().map(|i| i.schema_type.as_str()).collect();
        assert_eq!(types, ["WebPage", "Article"]);
        assert_eq!(
            items[0].data,
            serde_json::json!({"@type": "WebPage", "name": "Page"})
        );
        assert_eq!(
            items[1].data,
            serde_json::json!({
                "@type": "Article",
                "headline": "Post",
                "author": {"@type": "Person", "name": "Ada"}
            })
        );
    }

    #[test]
    fn microdata_values_names_and_ids() {
        let items = microdata(
            r#"<div itemscope itemtype="http://schema.org/Product https://example.com/Other"
                    itemid="urn:sku:42">
                <span itemprop="name   alternateName">  Blue
                    kettle </span>
                <span itemprop="price" content="12.50">12,50 €</span>
                <a itemprop="url" href="/kettle">see</a>
                <img itemprop="image" src="/a.png"><img itemprop="image" src="/b.png">
                <img itemprop="image" src="/c.png">
                <time itemprop="releaseDate" datetime="2024-01-15">15 January</time>
            </div>
            <div itemscope><span itemprop="name">no type</span></div>"#,
        );
        assert_eq!(
            items.len(),
            1,
            "an item without a type is not schema.org data"
        );
        assert_eq!(
            items[0].data,
            serde_json::json!({
                "@type": "Product",
                "@id": "urn:sku:42",
                "name": "Blue kettle",
                "alternateName": "Blue kettle",
                "price": "12.50",
                "url": "/kettle",
                "image": ["/a.png", "/b.png", "/c.png"],
                "releaseDate": "2024-01-15"
            })
        );
    }

    #[test]
    fn microdata_follows_itemref() {
        let items = microdata(
            r#"<div itemscope itemtype="https://schema.org/Person" itemref="contact missing">
                <span itemprop="name">Ada</span>
            </div>
            <p>Unrelated <span itemprop="ignored">text</span></p>
            <div id="contact"><span itemprop="email">ada@example.com</span></div>"#,
        );
        assert_eq!(
            items[0].data,
            serde_json::json!({"@type": "Person", "name": "Ada", "email": "ada@example.com"})
        );
    }

    /// Two items that name each other with `itemref`, and items nested past
    /// the depth limit: the extraction ends, with what it could read.
    #[test]
    fn microdata_loops_and_deep_nesting_end() {
        let items = microdata(
            r#"<div id="a" itemscope itemtype="https://schema.org/Person" itemref="b">
                <span itemprop="name">A</span></div>
            <div id="b" itemprop="knows" itemscope itemtype="https://schema.org/Person" itemref="a">
                <span itemprop="name">B</span></div>"#,
        );
        assert_eq!(items[0].data["knows"]["name"], "B");

        let depth = 20_000;
        let mut html = String::from(r#"<div itemscope itemtype="https://schema.org/Thing">"#);
        for _ in 0..depth {
            html.push_str(r#"<div itemprop="p" itemscope>"#);
        }
        html.push_str(r#"<span itemprop="name">leaf</span>"#);
        let items = microdata(&html);
        assert_eq!(items.len(), 1);
        let mut value = &items[0].data;
        let mut levels = 0;
        while let Some(next) = value.get("p") {
            value = next;
            levels += 1;
        }
        assert_eq!(levels, MICRODATA_MAX_DEPTH - 1);

        // Wide and deep without items: the ancestor walks are bounded too.
        let mut html = "<div>".repeat(20_000);
        html.push_str(
            &r#"<i itemprop="x" itemscope itemtype="https://schema.org/Thing"></i>"#.repeat(20_000),
        );
        assert!(microdata(&html).len() <= 20_000);
    }

    #[test]
    fn microdata_honors_only_types_and_convert_dates() {
        let html = r#"<div itemscope itemtype="https://schema.org/Article">
                <meta itemprop="datePublished" content="2024-01-15T10:00:00Z">
            </div>
            <div itemscope itemtype="https://schema.org/Organization">
                <span itemprop="name">Org</span>
            </div>"#;
        let extractor = SchemaExtractor::new(SchemaConfig {
            only_types: HashSet::from(["Article".to_string()]),
            convert_dates: true,
            ..Default::default()
        });
        let items = extractor.extract(html).unwrap().microdata;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].data["datePublished"], 1705312800);
    }

    #[test]
    fn merge_microdata_skips_types_the_json_ld_has() {
        let item = |t: &str, name: &str| {
            SchemaItem::new(t.to_string(), serde_json::json!({"@type": t, "name": name}))
        };
        let json_ld = serde_json::json!({
            "@context": "https://schema.org",
            "@graph": [{"@type": "https://schema.org/Article", "headline": "h"}]
        });

        // Nothing to add: the JSON-LD is returned as it was.
        assert_eq!(
            merge_microdata(Some(json_ld.clone()), vec![item("Article", "dup")]),
            Some(json_ld.clone())
        );
        assert_eq!(merge_microdata(None, vec![]), None);

        // A single Microdata item is the field; several values are an array.
        assert_eq!(
            merge_microdata(None, vec![item("Product", "p")]),
            Some(serde_json::json!({"@type": "Product", "name": "p"}))
        );
        assert_eq!(
            merge_microdata(
                Some(json_ld.clone()),
                vec![item("Article", "dup"), item("Product", "p")]
            ),
            Some(serde_json::json!([json_ld, {"@type": "Product", "name": "p"}]))
        );
        assert_eq!(
            merge_microdata(
                Some(serde_json::json!([{"@type": ["Person", "Author"]}, {"@type": "Thing"}])),
                vec![item("Author", "dup"), item("Event", "e")]
            ),
            Some(serde_json::json!([
                {"@type": ["Person", "Author"]},
                {"@type": "Thing"},
                {"@type": "Event", "name": "e"}
            ]))
        );
    }

    #[test]
    fn test_get_by_type() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <script type="application/ld+json">
                {
                    "@context": "https://schema.org",
                    "@graph": [
                        { "@type": "Article", "headline": "Article 1" },
                        { "@type": "Article", "headline": "Article 2" }
                    ]
                }
                </script>
            </head>
            <body></body>
            </html>
        "#;

        let extractor = SchemaExtractor::default();
        let result = extractor.extract(html).unwrap();

        let articles = extractor.get_by_type(&result, "Article");
        assert_eq!(articles.len(), 2);
    }

    #[test]
    fn test_convert_dates() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <script type="application/ld+json">
                {
                    "@context": "https://schema.org",
                    "@type": "Article",
                    "datePublished": "2024-01-15T10:00:00Z"
                }
                </script>
            </head>
            <body></body>
            </html>
        "#;

        let extractor = SchemaExtractor::new(SchemaConfig {
            convert_dates: true,
            ..Default::default()
        });
        let result = extractor.extract(html).unwrap();

        if let Value::Object(obj) = &result.items[0].data {
            // Should be converted to timestamp
            assert!(obj.get("datePublished").unwrap().is_number());
        }
    }

    #[test]
    fn test_multiple_json_ld_scripts() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <script type="application/ld+json">
                { "@type": "Article", "headline": "First" }
                </script>
                <script type="application/ld+json">
                { "@type": "Organization", "name": "Second" }
                </script>
            </head>
            <body></body>
            </html>
        "#;

        let extractor = SchemaExtractor::default();
        let result = extractor.extract(html).unwrap();

        assert_eq!(result.json_ld_count, 2);
        assert_eq!(result.items.len(), 2);
    }
}
