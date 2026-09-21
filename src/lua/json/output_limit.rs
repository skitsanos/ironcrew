use mlua::Result as LuaResult;
use std::io::{self, Write};

#[derive(Debug)]
struct LimitedWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(bytes.len()) > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("JSON output limit exceeded"));
        }
        self.written += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn ensure_output_fits(value: &serde_json::Value, max_bytes: usize) -> LuaResult<()> {
    let mut writer = LimitedWriter {
        written: 0,
        limit: max_bytes,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(mlua::Error::external(format!(
                "Lua/JSON conversion exceeded serialized output limit of {max_bytes} bytes"
            )));
        }
        return Err(mlua::Error::external(format!(
            "Failed to measure serialized JSON output: {error}"
        )));
    }
    Ok(())
}
