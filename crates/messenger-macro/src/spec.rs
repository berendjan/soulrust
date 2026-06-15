//! DSL parsing and validation — the equivalent of `spec.go` in the Go tools.

use std::collections::HashSet;

use quote::ToTokens;
use syn::braced;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, Token, Type};

/// A message a component sends or handles. `response` of `None` marks an
/// event (1:N fan-out, no return value), mirroring the Go `MessageDef`.
pub struct MessageDef {
    pub message: Type,
    pub response: Option<Type>,
}

impl MessageDef {
    /// Textual identity used for routing, like the Go tools match messages
    /// by their YAML string. Two components must spell the type the same way.
    pub fn key(&self) -> String {
        self.message.to_token_stream().to_string()
    }
}

pub struct Component {
    pub name: Ident,
    pub sends: Vec<MessageDef>,
    pub handles: Vec<MessageDef>,
}

pub struct Spec {
    pub messenger_name: Ident,
    pub components: Vec<Component>,
}

impl Parse for MessageDef {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let message: Type = input.parse()?;
        let response = if input.peek(Token![->]) {
            input.parse::<Token![->]>()?;
            Some(input.parse::<Type>()?)
        } else {
            None
        };
        input.parse::<Token![;]>()?;
        Ok(MessageDef { message, response })
    }
}

impl Parse for Component {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kw: Ident = input.parse()?;
        if kw != "component" {
            return Err(syn::Error::new(kw.span(), "expected `component`"));
        }
        let name: Ident = input.parse()?;

        let body;
        braced!(body in input);

        let mut sends = Vec::new();
        let mut handles = Vec::new();
        while !body.is_empty() {
            let kw: Ident = body.parse()?;
            if kw == "sends" {
                sends.push(body.parse()?);
            } else if kw == "handles" {
                handles.push(body.parse()?);
            } else {
                return Err(syn::Error::new(kw.span(), "expected `sends` or `handles`"));
            }
        }

        Ok(Component { name, sends, handles })
    }
}

impl Parse for Spec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kw: Ident = input.parse()?;
        if kw != "messenger" {
            return Err(syn::Error::new(
                kw.span(),
                "expected `messenger <Name>;` as the first declaration",
            ));
        }
        let messenger_name: Ident = input.parse()?;
        input.parse::<Token![;]>()?;

        let mut components = Vec::new();
        while !input.is_empty() {
            components.push(input.parse()?);
        }

        Ok(Spec { messenger_name, components })
    }
}

impl Spec {
    /// Component names (in declaration order) that handle the given message.
    pub fn handlers_for(&self, key: &str) -> Vec<&Component> {
        self.components
            .iter()
            .filter(|c| c.handles.iter().any(|h| h.key() == key))
            .collect()
    }

    /// The same checks as `Spec.Validate()` in the Go tools, reported as
    /// compile errors with spans on the offending declarations.
    pub fn validate(&self) -> syn::Result<()> {
        if self.components.is_empty() {
            return Err(syn::Error::new(
                self.messenger_name.span(),
                "at least one component is required",
            ));
        }

        let mut names = HashSet::new();
        for c in &self.components {
            if !names.insert(c.name.to_string()) {
                return Err(syn::Error::new(
                    c.name.span(),
                    format!("duplicate component name `{}`", c.name),
                ));
            }
            if c.sends.is_empty() && c.handles.is_empty() {
                return Err(syn::Error::new(
                    c.name.span(),
                    format!("component `{}` must declare at least one `sends` or `handles`", c.name),
                ));
            }
            for (list, label) in [(&c.sends, "sends"), (&c.handles, "handles")] {
                let mut seen = HashSet::new();
                for msg in list.iter() {
                    if !seen.insert(msg.key()) {
                        return Err(syn::Error::new_spanned(
                            &msg.message,
                            format!("component `{}`: duplicate message in `{label}`", c.name),
                        ));
                    }
                }
            }
        }

        // 1:1 sends (with a response type) require exactly one handler.
        for c in &self.components {
            for send in &c.sends {
                if send.response.is_none() {
                    continue; // events may have 0..N handlers
                }
                let handlers = self.handlers_for(&send.key());
                if handlers.is_empty() {
                    return Err(syn::Error::new_spanned(
                        &send.message,
                        format!(
                            "component `{}`: message `{}` has a response type but no component handles it",
                            c.name,
                            send.key(),
                        ),
                    ));
                }
                if handlers.len() > 1 {
                    let names: Vec<String> =
                        handlers.iter().map(|h| h.name.to_string()).collect();
                    return Err(syn::Error::new_spanned(
                        &send.message,
                        format!(
                            "component `{}`: message `{}` has a response type but multiple handlers: {} (1:1 routing requires exactly one)",
                            c.name,
                            send.key(),
                            names.join(", "),
                        ),
                    ));
                }
            }
        }

        Ok(())
    }
}
