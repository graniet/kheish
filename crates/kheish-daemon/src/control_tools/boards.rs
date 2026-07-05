//! Tool that lets agents draw vector elements onto a shared board.

use anyhow::Result;
use async_trait::async_trait;
use kheish_runtime::{
    SandboxProfile, Tool, ToolContext, ToolDescriptor, ToolExecutionOutput, ToolSchema,
};
use serde::Deserialize;
use serde_json::Value;

use super::DaemonToolControlHandle;
use super::helpers::{
    build_array_field, build_string_field, deserialize_tool_request, execution_run_id,
    execution_session_id,
};

#[derive(Clone)]
pub(crate) struct BoardDrawTool {
    control: DaemonToolControlHandle,
}

impl BoardDrawTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[derive(Debug, Deserialize)]
struct BoardDrawRequest {
    board_id: String,
    elements: Vec<Value>,
    #[serde(default)]
    note: Option<String>,
}

#[async_trait]
impl Tool for BoardDrawTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "board_draw".to_string(),
            description: concat!(
                "Draw on a shared whiteboard: append vector elements as a new board revision, ",
                "stamped with your name and color. Each element is an object with kind ",
                "path|line|arrow|rect|ellipse|text plus its geometry: path {points:[[x,y],..]}, ",
                "line/arrow {from:[x,y], to:[x,y]}, rect/ellipse {x,y,w,h}, text {x,y,text,font_size}. ",
                "Optional per element: color \"#RRGGBB\", stroke_width. Coordinates are pixels ",
                "from the canvas top-left."
            )
            .to_string(),
            schema: ToolSchema {
                fields: vec![
                    build_string_field("board_id", "The daemon-owned board identifier.", true),
                    build_array_field(
                        "elements",
                        "The vector elements to draw, as objects described above.",
                        true,
                        kheish_runtime::ToolInputKind::Object,
                    ),
                    build_string_field(
                        "note",
                        "Optional short note recorded on the revision.",
                        false,
                    ),
                ],
            },
            timeout_ms: 60_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: false,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let run_id = execution_run_id(&ctx).map(ToOwned::to_owned);
        let request = deserialize_tool_request::<BoardDrawRequest>(input)?;
        let elements = request
            .elements
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| anyhow::anyhow!("invalid board element: {error}"))?;
        let control = self.control.resolve()?;
        let revision = control
            .agent_draw_on_board(
                &session_id,
                run_id.as_deref(),
                &request.board_id,
                elements,
                request.note,
            )
            .await?;
        Ok(ToolExecutionOutput::json(serde_json::json!({
            "board_id": revision.board_id,
            "revision_id": revision.revision_id,
            "render_asset_id": revision.render_asset_id,
            "note": revision.note,
        })))
    }
}
