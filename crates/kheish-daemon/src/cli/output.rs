//! Output rendering helpers for the CLI binary.

use anyhow::Result;
use serde::Serialize;

/// Renders structured CLI responses according to the selected output format.
pub(crate) struct Printer {
    pub(crate) format: crate::OutputFormat,
}

impl Printer {
    /// Prints one serializable value using the configured output format.
    pub(crate) fn print<T>(&self, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        let rendered = match self.format {
            crate::OutputFormat::Pretty => serde_json::to_string_pretty(value)?,
            crate::OutputFormat::Json => serde_json::to_string(value)?,
        };
        println!("{rendered}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Printer;

    #[test]
    fn printer_serializes_json_without_panicking() {
        let printer = Printer {
            format: crate::OutputFormat::Json,
        };
        printer
            .print(&serde_json::json!({ "ok": true }))
            .expect("printer should serialize JSON");
    }

    #[test]
    fn printer_serializes_pretty_output_without_panicking() {
        let printer = Printer {
            format: crate::OutputFormat::Pretty,
        };
        printer
            .print(&serde_json::json!({ "ok": true }))
            .expect("printer should serialize pretty output");
    }
}
