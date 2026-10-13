use crate::{Server, State, sandbox::Sandbox};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::Read;
pub async fn refresh(state: &State, server: &Server, id: &str) -> Result<Value> {
    let response = state.remote(&format!("servers/{id}")).await?;
    ensure!(
        response["settings"]["uuid"] == id,
        "Panel returned a different server"
    );
    let mut settings = response["settings"].clone();
    settings["_process_configuration"] = response["process_configuration"].clone();
    *server.settings.write().await = settings.clone();
    Ok(settings)
}
pub fn ready(line: &str, process: &Value) -> bool {
    let text = if process["startup"]["strip_ansi"].as_bool().unwrap_or(true) {
        regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]")
            .unwrap()
            .replace_all(line, "")
            .into_owned()
    } else {
        line.to_owned()
    };
    process["startup"]["done"].as_array().is_none_or(|done| {
        done.is_empty()
            || done.iter().any(|pattern| {
                pattern.as_str().is_some_and(|p| {
                    if let Some(p) = p.strip_prefix("regex:") {
                        regex::RegexBuilder::new(p)
                            .size_limit(1024 * 1024)
                            .build()
                            .is_ok_and(|r| r.is_match(&text))
                    } else {
                        text.contains(p)
                    }
                })
            })
    })
}
pub async fn apply_engine(engine: &crate::engine::Engine, settings: &Value) -> Result<()> {
    let id = settings["uuid"].as_str().context("Missing UUID")?;
    let mut trusted = settings.clone();
    trusted["egg"]["file_denylist"] = json!([]);
    let sb = Sandbox::new(&engine.directory(id)?, &trusted, &engine.config)?;
    let mut process = settings["_process_configuration"].clone();
    resolve_placeholders(&mut process, &engine.config)?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(configs) = process["configs"].as_array() {
            ensure!(configs.len() <= 1024, "Too many egg configuration files");
            for config in configs {
                let name = config["file"]
                    .as_str()
                    .context("Configuration filename missing")?;
                let parser = config["parser"]
                    .as_str()
                    .context("Configuration parser missing")?;
                let mut text = String::new();
                match sb.open(name, false, false, false, false) {
                    Ok(file) => {
                        file.take(4 * 1024 * 1024 + 1).read_to_string(&mut text)?;
                        ensure!(
                            text.len() <= 4 * 1024 * 1024,
                            "Configuration file exceeds limit"
                        );
                    }
                    Err(_) if !sb.root.try_exists(crate::sandbox::relative(name, false)?)? => {
                        if parser == "file" {
                            continue;
                        }
                    }
                    Err(e) => return Err(e),
                }
                let replacement = config["replace"]
                    .as_array()
                    .context("Invalid replacement rules")?;
                ensure!(replacement.len() <= 1024, "Too many replacement rules");
                let updated = render(parser, &text, replacement)?;
                sb.write(name, &mut updated.as_bytes(), 4 * 1024 * 1024, false)?;
            }
        }
        Ok(())
    })
    .await?
}
fn scalar(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        if value.is_null() {
            String::new()
        } else {
            value.to_string()
        }
    })
}
fn condition(old: &str, rule: &Value) -> bool {
    rule.get("if_value").is_none_or(|v| scalar(v) == old)
}
pub fn render(parser: &str, text: &str, rules: &[Value]) -> Result<String> {
    match parser {
        "json" | "yaml" | "yml" => {
            let mut value: Value = if text.trim().is_empty() {
                json!({})
            } else if parser == "json" {
                serde_json::from_str(text)?
            } else {
                serde_yaml::from_str(text)
                    .map_err(|_| anyhow::anyhow!("Invalid YAML egg configuration"))?
            };
            for rule in rules {
                let key = scalar(&rule["match"]);
                let parts: keypath::Parts = keypath::parse(&key)?;
                patch(&mut value, &parts, rule, 0)?;
            }
            Ok(if parser == "json" {
                serde_json::to_string_pretty(&value)? + "\n"
            } else {
                serde_yaml::to_string(&value)?
            })
        }
        "properties" | "ini" | "file" => {
            let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
            for rule in rules {
                let key = scalar(&rule["match"]);
                ensure!(key.len() <= 4096, "Replacement key too long");
                let replacement = scalar(&rule["replace_with"]);
                ensure!(
                    !replacement.contains(['\0', '\r', '\n']),
                    "Multiline replacement is not allowed"
                );
                let (section, key) = if parser == "ini" {
                    key.split_once('.')
                        .map(|(a, b)| (Some(a.to_owned()), b.to_owned()))
                        .unwrap_or((None, key))
                } else {
                    (None, key)
                };
                let mut current = None;
                let mut replaced = false;
                for line in &mut lines {
                    let trimmed = line.trim();
                    if parser == "ini" && trimmed.starts_with('[') && trimmed.ends_with(']') {
                        current = Some(trimmed[1..trimmed.len() - 1].to_owned());
                        continue;
                    }
                    if section != current && parser == "ini" {
                        continue;
                    }
                    if parser == "file" {
                        if let Some(regex) = key.strip_prefix("regex:") {
                            let regex = regex::RegexBuilder::new(regex)
                                .size_limit(1024 * 1024)
                                .build()?;
                            if regex.is_match(line) && condition(line, rule) {
                                *line = regex.replace_all(line, replacement.as_str()).into_owned();
                                replaced = true;
                            }
                        } else if line.starts_with(&key) && condition(line, rule) {
                            *line = replacement.clone();
                            replaced = true;
                        }
                    } else if !trimmed.starts_with(['#', ';'])
                        && let Some((name, old)) =
                            trimmed.split_once('=').or_else(|| trimmed.split_once(':'))
                        && name.trim() == key
                        && condition(old.trim(), rule)
                    {
                        *line = format!("{key}={replacement}");
                        replaced = true;
                    }
                }
                if !replaced && parser != "file" && rule.get("if_value").is_none() {
                    if let Some(section) = section {
                        let header = format!("[{section}]");
                        if let Some(i) = lines.iter().position(|l| l.trim() == header) {
                            lines.insert(i + 1, format!("{key}={replacement}"));
                        } else {
                            lines.push(header);
                            lines.push(format!("{key}={replacement}"));
                        }
                    } else {
                        lines.push(format!("{key}={replacement}"));
                    }
                }
            }
            Ok(lines.join("\n") + "\n")
        }
        "xml" => xml(text, rules),
        _ => anyhow::bail!("Unsupported egg parser: {parser}"),
    }
}
mod keypath {
    use super::*;
    pub type Parts = Vec<String>;
    pub fn parse(raw: &str) -> Result<Parts> {
        ensure!(raw.len() <= 4096, "Key path too long");
        let mut out = Vec::new();
        let mut current = String::new();
        let mut escape = false;
        for c in raw.chars() {
            if escape {
                current.push(c);
                escape = false
            } else if c == '\\' {
                escape = true
            } else if c == '.' {
                ensure!(!current.is_empty(), "Empty key path");
                out.push(std::mem::take(&mut current))
            } else {
                current.push(c)
            }
        }
        ensure!(
            !escape && !current.is_empty() && out.len() < 128,
            "Invalid key path"
        );
        out.push(current);
        Ok(out)
    }
}
fn patch(value: &mut Value, parts: &[String], rule: &Value, depth: usize) -> Result<()> {
    ensure!(depth <= 128, "Key path too deep");
    if parts.is_empty() {
        if condition(&scalar(value), rule) {
            *value = rule["replace_with"].clone();
        }
        return Ok(());
    }
    let key = &parts[0];
    if key == "*" {
        match value {
            Value::Object(map) => {
                for child in map.values_mut() {
                    patch(child, &parts[1..], rule, depth + 1)?
                }
            }
            Value::Array(list) => {
                for child in list {
                    patch(child, &parts[1..], rule, depth + 1)?
                }
            }
            _ => {}
        }
        return Ok(());
    }
    if let Value::Array(list) = value {
        let i = key.parse::<usize>()?;
        ensure!(i < list.len(), "Array index outside configuration");
        return patch(&mut list[i], &parts[1..], rule, depth + 1);
    }
    if value.is_null() {
        *value = json!({})
    }
    let map = value
        .as_object_mut()
        .context("Key path traverses a scalar")?;
    patch(
        map.entry(key.clone()).or_insert(Value::Null),
        &parts[1..],
        rule,
        depth + 1,
    )
}
fn xml(text: &str, rules: &[Value]) -> Result<String> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut depth = 0u32;
    loop {
        match reader.read_event()? {
            quick_xml::events::Event::Start(_) => {
                depth += 1;
                ensure!(depth <= 128, "XML depth exceeds limit")
            }
            quick_xml::events::Event::End(_) => {
                depth = depth.checked_sub(1).context("Invalid XML nesting")?
            }
            quick_xml::events::Event::DocType(_) => {
                anyhow::bail!("XML external entities are forbidden")
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }

    ensure!(
        !text.contains("<!DOCTYPE") && !text.contains("<!ENTITY"),
        "XML entity declarations are forbidden"
    );
    let package = if text.trim().is_empty() {
        sxd_document::Package::new()
    } else {
        sxd_document::parser::parse(text)
            .map_err(|_| anyhow::anyhow!("Invalid XML configuration"))?
    };
    let doc = package.as_document();
    for rule in rules {
        let key = scalar(&rule["match"]);
        let parts = keypath::parse(&key)?;
        ensure!(parts.len() <= 128, "XML key path too deep");
        if !key.contains(['*', '[', '@']) {
            let root_name = &parts[0];
            let mut element = doc.root().children().iter().find_map(|c| c.element());
            if element.is_none() {
                let e = doc.create_element(root_name.as_str());
                doc.root().append_child(e);
                element = Some(e)
            }
            let mut e = element.context("XML root missing")?;
            ensure!(e.name().local_part() == root_name, "XML root mismatch");
            for tag in &parts[1..] {
                let child = e
                    .children()
                    .iter()
                    .find_map(|c| c.element().filter(|e| e.name().local_part() == tag));
                e = if let Some(child) = child {
                    child
                } else {
                    let child = doc.create_element(tag.as_str());
                    e.append_child(child);
                    child
                };
            }
        }
        let expression = format!("/{}", parts.join("/"));
        let xpath = sxd_xpath::Factory::new()
            .build(&expression)?
            .context("Invalid XML path")?;
        let value = xpath.evaluate(&sxd_xpath::Context::new(), doc.root())?;
        if let sxd_xpath::Value::Nodeset(nodes) = value {
            for node in nodes.document_order() {
                if condition(&node.string_value(), rule) {
                    let replacement = scalar(&rule["replace_with"]);
                    if let Some(e) = node.element() {
                        e.clear_children();
                        e.append_child(doc.create_text(&replacement));
                    } else if let Some(a) = node.attribute() {
                        a.parent()
                            .context("Missing XML parent")?
                            .set_attribute_value(a.name(), &replacement);
                    }
                }
            }
        }
    }
    let mut output = Vec::new();
    sxd_document::writer::format_document(&doc, &mut output)?;
    Ok(String::from_utf8(output)?)
}

fn resolve_placeholders(value: &mut Value, config: &crate::config::Config) -> Result<()> {
    match value {
        Value::String(text) => {
            let pattern = regex::Regex::new(r"\{\{config\.([a-zA-Z0-9_.-]+)\}\}")?;
            let original = text.clone();
            let mut rejected = false;
            *text = pattern
                .replace_all(&original, |captures: &regex::Captures| match &captures[1] {
                    "docker.network.interface" => config
                        .docker
                        .network
                        .gateway
                        .clone()
                        .unwrap_or_else(|| "172.17.0.1".into()),
                    "system.sftp.bind_port" => config.system.sftp.bind_port.to_string(),
                    "api.port" => config.api.port.to_string(),
                    _ => {
                        rejected = true;
                        String::new()
                    }
                })
                .into_owned();
            ensure!(
                !rejected,
                "Egg requested an unsupported or private node configuration placeholder"
            );
        }
        Value::Array(values) => {
            for v in values {
                resolve_placeholders(v, config)?
            }
        }
        Value::Object(values) => {
            for v in values.values_mut() {
                resolve_placeholders(v, config)?
            }
        }
        _ => {}
    }
    Ok(())
}
