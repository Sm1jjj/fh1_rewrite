//! Minimal XML -> JSON: each element becomes an object of its attributes (numbers parsed)
//! plus its child elements by name (an array when a name repeats).

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde_json::{Map, Value};

use anyhow::Result;

pub fn to_json(xml: &str) -> Result<Value> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    // Stack of (name, object) for open elements.
    let mut stack: Vec<(String, Map<String, Value>)> = vec![(String::new(), Map::new())];
    loop {
        match reader.read_event()? {
            Event::Start(e) => stack.push(open(&e)?),
            Event::Empty(e) => {
                let (name, obj) = open(&e)?;
                insert(&mut stack.last_mut().unwrap().1, name, Value::Object(obj));
            }
            Event::End(_) => {
                let (name, obj) = stack.pop().unwrap();
                insert(&mut stack.last_mut().unwrap().1, name, Value::Object(obj));
            }
            Event::Text(t) => {
                let text = t.xml10_content().into_owned();
                if !text.is_empty() {
                    stack.last_mut().unwrap().1.insert("#text".into(), scalar(&text));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(Value::Object(stack.pop().unwrap().1))
}

fn open(e: &BytesStart) -> Result<(String, Map<String, Value>)> {
    let name = AsRef::<str>::as_ref(&e.name()).to_owned();
    let mut obj = Map::new();
    for a in e.attributes() {
        let a = a?;
        let key = AsRef::<str>::as_ref(&a.key).to_owned();
        obj.insert(key, scalar(&a.normalized_value(XmlVersion::Implicit1_0)?));
    }
    Ok((name, obj))
}

fn insert(parent: &mut Map<String, Value>, name: String, v: Value) {
    match parent.get_mut(&name) {
        None => {
            parent.insert(name, v);
        }
        Some(Value::Array(arr)) => arr.push(v),
        Some(existing) => {
            let prev = existing.take();
            *existing = Value::Array(vec![prev, v]);
        }
    }
}

fn scalar(s: &str) -> Value {
    s.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(s.to_owned()))
}
