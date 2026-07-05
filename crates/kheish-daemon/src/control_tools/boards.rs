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

#[derive(Debug, Deserialize)]
struct BoardViewRequest {
    board_id: String,
}

#[async_trait]
impl Tool for BoardDrawTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "board_draw".to_string(),
            description: concat!(
                "Draw on a shared whiteboard: append vector elements as a new board revision, ",
                "stamped with your name and color. Call board_view first when a board may already ",
                "have content, so you continue the existing drawing instead of starting over. Draw ",
                "in SMALL batches of 3-8 elements and call board_draw several times: each call ",
                "becomes a revision humans watch appear live. Place new elements in free areas of ",
                "the occupancy map and align coordinates with the elements you saw. Each element is ",
                "an object with kind path|line|arrow|rect|ellipse|text plus its geometry: path ",
                "{points:[[x,y],..]}, line/arrow {from:[x,y], to:[x,y]}, rect/ellipse {x,y,w,h}, ",
                "text {x,y,text,font_size}. Optional per element: color \"#RRGGBB\", stroke_width. ",
                "Coordinates are pixels from the canvas top-left. The response returns the updated ",
                "board scene so you can keep drawing without another call."
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
        // Return the post-draw scene so the agent immediately sees the updated
        // board and can continue drawing in place without a separate view call.
        let mut scene = control.agent_view_board(&session_id, &request.board_id).await?;
        if let Value::Object(map) = &mut scene {
            map.insert("board_id".to_string(), serde_json::json!(revision.board_id));
            map.insert(
                "revision_id".to_string(),
                serde_json::json!(revision.revision_id),
            );
            map.insert(
                "render_asset_id".to_string(),
                serde_json::json!(revision.render_asset_id),
            );
            map.insert("note".to_string(), serde_json::json!(revision.note));
        }
        Ok(ToolExecutionOutput::json(scene))
    }
}

#[derive(Clone)]
pub(crate) struct BoardViewTool {
    control: DaemonToolControlHandle,
}

impl BoardViewTool {
    pub(crate) fn new(control: DaemonToolControlHandle) -> Self {
        Self { control }
    }
}

#[async_trait]
impl Tool for BoardViewTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "board_view".to_string(),
            description: concat!(
                "Inspect a shared whiteboard before drawing so you continue the existing sketch ",
                "instead of starting over. Returns the board metadata, the tip revision's author, ",
                "a compact list of placed elements with their coordinates, an ASCII occupancy map ",
                "showing which author owns each region of the canvas, and a hint naming the largest ",
                "free area. Read this first whenever a board may already have content, then align ",
                "your board_draw coordinates with what you see and place new elements in free cells."
            )
            .to_string(),
            schema: ToolSchema {
                fields: vec![build_string_field(
                    "board_id",
                    "The daemon-owned board identifier.",
                    true,
                )],
            },
            timeout_ms: 30_000,
            sandbox: SandboxProfile::Inherited,
            allows_parallel: true,
        }
    }

    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolExecutionOutput> {
        let session_id = execution_session_id(&ctx)?;
        let request = deserialize_tool_request::<BoardViewRequest>(input)?;
        let control = self.control.resolve()?;
        let scene = control
            .agent_view_board(session_id, &request.board_id)
            .await?;
        Ok(ToolExecutionOutput::json(scene))
    }
}
