//! Read-only Jira/Confluence Cloud adapters over their documented REST APIs.
//! Copies have explicit sharing; provider-wide ACL equivalence is not assumed.
use crate::{
    Error, Result,
    connectors::{Remote, pair, segment},
    model,
    store::s,
    string,
};
use serde_json::{Value, json};

pub fn validate(kind: &str, config: &Value) -> Result<()> {
    model::checked_url(string(config, "base_url", 2000)?)?;
    if !["bearer", "basic"].contains(&config["auth_type"].as_str().unwrap_or("bearer")) {
        return Err(Error::bad(
            "Choose OAuth/bearer or API-token authentication",
        ));
    }
    if kind == "jira" {
        string(config, "jql", 2000)?;
    } else if config["space_id"].as_str().is_none() {
        string(config, "space_key", 100)?;
    }
    if let Some(id) = config["space_id"].as_str()
        && (id.is_empty() || !id.bytes().all(|v| v.is_ascii_digit()))
    {
        return Err(Error::bad("Confluence space ID must be numeric"));
    }
    Ok(())
}
pub fn next_page(base: &str, next: &str) -> Result<(String, Vec<(String, String)>)> {
    let origin = model::checked_url(base)?;
    let url = if next.starts_with("/wiki/") && origin.path() != "/" {
        url::Url::parse(&format!("{}{}", base.trim_end_matches('/'), next))
    } else {
        origin.join(next)
    }
    .map_err(|_| Error::bad("Invalid provider pagination URL"))?;
    let prefix = format!("{}/", origin.path().trim_end_matches('/'));
    if url.origin() != origin.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with(&prefix)
        || url.as_str().len() > 8192
    {
        return Err(Error::bad(
            "Provider pagination left the configured API origin or path",
        ));
    }
    let path = url
        .path()
        .strip_prefix(&prefix)
        .ok_or_else(|| Error::bad("Invalid pagination path"))?
        .to_string();
    Ok((
        path,
        url.query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    ))
}
pub(crate) async fn listing(
    remote: &mut Remote<'_>,
    cursor: &Value,
) -> Result<(Vec<Value>, Value)> {
    let config = remote.connector["config"].clone();
    if remote.connector["kind"] == "jira" {
        let mut body = json!({"jql":config["jql"],"maxResults":100,"fields":["summary","description","status","assignee","reporter","created","updated"]});
        if let Some(next) = cursor["next"].as_str() {
            body["nextPageToken"] = json!(next);
        }
        let result = remote
            .request(
                reqwest::Method::POST,
                "rest/api/3/search/jql",
                &[],
                Some(body),
            )
            .await?;
        let issues = result["issues"]
            .as_array()
            .ok_or_else(|| Error::bad("Invalid Jira search response"))?
            .clone();
        let next = result["nextPageToken"].as_str().filter(|v| !v.is_empty());
        if result["isLast"] == false && next.is_none() {
            return Err(Error::bad("Jira omitted its continuation token"));
        }
        if next.is_some_and(|v| cursor["next"] == v) {
            return Err(Error::bad("Jira repeated a continuation token"));
        }
        return Ok((
            issues,
            next.map(|n| json!({"next":n})).unwrap_or(Value::Null),
        ));
    }
    let (path, query) = if let Some(next) = cursor["next"].as_str() {
        next_page(&remote.base, next)?
    } else {
        let space = if let Some(id) = config["space_id"].as_str() {
            id.to_string()
        } else {
            let result = remote
                .get(
                    "wiki/api/v2/spaces",
                    vec![
                        pair("keys", string(&config, "space_key", 100)?),
                        pair("limit", 2),
                    ],
                )
                .await?;
            let spaces = result["results"]
                .as_array()
                .filter(|v| v.len() == 1)
                .ok_or_else(|| Error::bad("Confluence space key did not resolve uniquely"))?;
            string(&spaces[0], "id", 100)?.to_string()
        };
        (
            format!("wiki/api/v2/spaces/{}/pages", segment(&space)),
            vec![
                pair("limit", 100),
                pair("status", "current"),
                pair("body-format", "storage"),
            ],
        )
    };
    let result = remote.get(&path, query).await?;
    let pages = result["results"]
        .as_array()
        .ok_or_else(|| Error::bad("Invalid Confluence pages response"))?
        .clone();
    let next = result["_links"]["next"].as_str().filter(|v| !v.is_empty());
    if next.is_some_and(|v| cursor["next"] == v) {
        return Err(Error::bad("Confluence repeated a continuation URL"));
    }
    if let Some(next) = next {
        next_page(&remote.base, next)?;
    }
    Ok((
        pages,
        next.map(|n| json!({"next":n})).unwrap_or(Value::Null),
    ))
}
pub fn adf_text(value: &Value) -> Result<String> {
    adf_text_spans(value).map(|(text, _)| text)
}

/// [`adf_text`] and the byte ranges of its text nodes that carry ADF's
/// `code` mark. The text is exactly `adf_text`'s: inline code is recorded
/// beside it, never written into it, so a document's sha256 and the memories
/// citing it do not change.
pub fn adf_text_spans(value: &Value) -> Result<(String, Vec<(usize, usize)>)> {
    type Spans = Vec<(usize, usize)>;
    fn walk(
        value: &Value,
        depth: usize,
        nodes: &mut usize,
        out: &mut String,
        spans: &mut Spans,
    ) -> Result<()> {
        *nodes += 1;
        // Earlier builds: 50,000 nodes and 512 KiB of text.
        let cap = crate::limits::current().connector_nodes;
        if depth > 40 || *nodes > cap {
            return Err(crate::connectors::too_large(
                &format!(
                    "a Jira document is nested deeper than 40 levels or has more than {} nodes",
                    crate::limits::grouped(cap)
                ),
                "connector_nodes",
                cap,
            ));
        }
        if out.len() > crate::documents::max_text() {
            return Err(text_limit(out.len()));
        }
        if value.is_null() {
            return Ok(());
        }
        if let Some(text) = value.as_str() {
            if out.len().saturating_add(text.len()) > crate::documents::max_text() {
                return Err(text_limit(out.len().saturating_add(text.len())));
            }
            out.push_str(text);
            return Ok(());
        }
        if let Some(items) = value.as_array() {
            for item in items {
                walk(item, depth + 1, nodes, out, spans)?;
            }
            return Ok(());
        }
        let kind = s(value, "type");
        match kind {
            "text" => {
                let start = out.len();
                out.push_str(s(value, "text"));
                if out.len() > start
                    && value["marks"]
                        .as_array()
                        .is_some_and(|marks| marks.iter().any(|mark| mark["type"] == "code"))
                {
                    spans.push((start, out.len()));
                }
            }
            "hardBreak" => out.push('\n'),
            "mention" => out.push_str(
                value["attrs"]["text"]
                    .as_str()
                    .or(value["attrs"]["id"].as_str())
                    .unwrap_or("[mention]"),
            ),
            "emoji" => out.push_str(
                value["attrs"]["text"]
                    .as_str()
                    .or(value["attrs"]["shortName"].as_str())
                    .unwrap_or("[emoji]"),
            ),
            "inlineCard" | "blockCard" => out.push_str(
                value["attrs"]["url"]
                    .as_str()
                    .unwrap_or("[linked content not fetched]"),
            ),
            "media" | "mediaSingle" => out.push_str("[attachment content not fetched]"),
            "listItem" => out.push_str("\n- "),
            "tableCell" | "tableHeader" => out.push_str(" | "),
            _ => {}
        }
        if let Some(content) = value.get("content") {
            walk(content, depth + 1, nodes, out, spans)?;
        }
        if [
            "paragraph",
            "heading",
            "codeBlock",
            "blockquote",
            "tableRow",
        ]
        .contains(&kind)
        {
            out.push('\n');
        }
        if out.len() > crate::documents::max_text() {
            return Err(text_limit(out.len()));
        }
        Ok(())
    }
    fn text_limit(bytes: usize) -> Error {
        crate::connectors::too_large(
            &format!("a Jira document has {} of text", crate::limits::size(bytes)),
            "document_text_bytes",
            crate::documents::max_text(),
        )
    }
    let mut text = String::new();
    let mut spans = Vec::new();
    let mut nodes = 0;
    walk(value, 0, &mut nodes, &mut text, &mut spans)?;
    Ok((text, spans))
}
pub(crate) async fn document(remote: &mut Remote<'_>, item: &Value) -> Result<Option<Value>> {
    if remote.connector["kind"] == "confluence" {
        let id = string(item, "id", 100)?;
        let page = remote
            .get(
                &format!("wiki/api/v2/pages/{}", segment(id)),
                vec![pair("body-format", "storage")],
            )
            .await?;
        if page["status"].as_str().is_some_and(|v| v != "current") {
            return Ok(None);
        }
        // The page's storage HTML is bounded as a document; its text by
        // `limits.document_text_bytes`.
        let html = page["body"]["storage"]["value"]
            .as_str()
            .ok_or_else(|| Error::bad("Missing Confluence page body"))?;
        if html.len() > crate::documents::max_binary() {
            return Err(crate::connectors::too_large(
                &format!(
                    "Confluence page {id} has {} of storage HTML",
                    crate::limits::size(html.len())
                ),
                "document_bytes",
                crate::documents::max_binary(),
            ));
        }
        let parsed = crate::documents::extract("page.html", html.as_bytes()).map_err(|error| {
            if error.message.starts_with("Extracted ") {
                crate::connectors::too_large(
                    &format!("Confluence page {id}: {}", error.message),
                    "document_text_bytes",
                    crate::documents::max_text(),
                )
            } else {
                error
            }
        })?;
        let content = format!(
            "# {}\nCreated: {}\nUpdated: {}\nVersion author: {}\n\n{}",
            s(&page, "title"),
            s(&page, "createdAt"),
            s(&page["version"], "createdAt"),
            s(&page["version"], "authorId"),
            s(&parsed, "text")
        );
        // Inline `<code>` reaches the text as backticks (html2text's plain
        // rendering), which mark code for the mention resolver as they do in
        // Markdown: the page needs no recorded spans.
        let site = remote.connector["config"]["site_url"]
            .as_str()
            .unwrap_or(&remote.base);
        let web = page["_links"]["webui"]
            .as_str()
            .and_then(|v| url::Url::parse(site).ok()?.join(v).ok())
            .map(|u| u.to_string());
        return Ok(Some(
            json!({"id":id,"title":page["title"],"content":content,"url":web,"source_timestamp":null,"metadata":{"version":page["version"],"created_at":page["createdAt"],"format":"storage_html","attachments_fetched":false,"macros_expanded":false}}),
        ));
    }
    let key = string(item, "key", 100)?;
    let fields = &item["fields"];
    let (description, mut spans) = adf_text_spans(&fields["description"])?;
    let mut text = format!(
        "# {key}: {}\nReporter: {} ({})\nAssignee: {} ({})\nCreated: {}\nUpdated: {}\nStatus: {}\n\n",
        s(fields, "summary"),
        s(&fields["reporter"], "displayName"),
        s(&fields["reporter"], "accountId"),
        s(&fields["assignee"], "displayName"),
        s(&fields["assignee"], "accountId"),
        s(fields, "created"),
        s(fields, "updated"),
        s(&fields["status"], "name"),
    );
    // The description and each comment body follow their headers; their
    // code spans move with them.
    let shift = |spans: Vec<(usize, usize)>, base: usize| {
        spans.into_iter().map(move |(a, b)| (base + a, base + b))
    };
    spans = shift(spans, text.len()).collect();
    text.push_str(&description);
    let mut offset = 0;
    let mut comments = 0;
    loop {
        let result = remote
            .get(
                &format!("rest/api/3/issue/{}/comment", segment(key)),
                vec![pair("startAt", offset), pair("maxResults", 100)],
            )
            .await?;
        let page = result["comments"]
            .as_array()
            .ok_or_else(|| Error::bad("Invalid Jira comment page"))?;
        let total = result["total"]
            .as_u64()
            .ok_or_else(|| Error::bad("Jira comment total missing"))? as usize;
        let cap = crate::limits::current().connector_documents;
        if total > cap {
            return Err(crate::connectors::too_large(
                &format!(
                    "a Jira issue has {} comments",
                    crate::limits::grouped(total)
                ),
                "connector_documents",
                cap,
            ));
        }
        if page.is_empty() && offset < total {
            return Err(Error::bad("Jira returned an incomplete comment page"));
        }
        for comment in page {
            let (body, marked) = adf_text_spans(&comment["body"])?;
            text.push_str(&format!(
                "\nComment by {} ({}) at {} (updated {})\n",
                s(&comment["author"], "displayName"),
                s(&comment["author"], "accountId"),
                s(comment, "created"),
                s(comment, "updated"),
            ));
            spans.extend(shift(marked, text.len()));
            text.push_str(&body);
            comments += 1;
        }
        if text.len() > crate::documents::max_text() {
            return Err(crate::connectors::too_large(
                &format!(
                    "a Jira issue has {} of text",
                    crate::limits::size(text.len())
                ),
                "document_text_bytes",
                crate::documents::max_text(),
            ));
        }
        offset += page.len();
        if offset >= total {
            break;
        }
    }
    let web = remote.connector["config"]["site_url"]
        .as_str()
        .map(|base| format!("{}/browse/{key}", base.trim_end_matches('/')));
    let mut document = json!({"id":key,"title":format!("{key}: {}",s(fields,"summary")),"content":text,"url":web,"source_timestamp":null,"metadata":{"created_at":fields["created"],"updated_at":fields["updated"],"comments":comments,"format":"atlassian_document_format","attachments_fetched":false}});
    if let Some(spans) = crate::doc_links::spans_json(&spans) {
        document["code_spans"] = spans;
    }
    Ok(Some(document))
}
