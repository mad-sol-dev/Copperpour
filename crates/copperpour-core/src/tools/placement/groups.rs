//! `group_footprints`: make footprints one KiCad board group.
//!
//! `group_components` only writes a `Group` field on schematic symbols, and
//! KiCad never treats that field as a group. This tool creates the board's own
//! `(group …)`, the one KiCad selects, moves and locks as a block (Ctrl+G in
//! the PCB editor). When KiCad holds the board it is created over IPC in one
//! undo step; otherwise the closed board file gets the same entry.
//!
//! A footprint that is already a member of a group is refused, not moved.
//! KiCad keeps an item in one group only, so adding it here would take it out
//! of a group the caller never named.
//!
//! Every success field is read back: over IPC from the group KiCad holds after
//! the commit, on the file path from the board text that was written.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::pcb_board::{attempt_ipc_write_in, BoardWrite};
use crate::tools::pcb_components::{
    check_single_board_form, footprint_reference, ClosedBoardError,
};
use crate::tools::{get_path, invalid_arg, require_array, require_str, ToolContext, ToolDef};
use copperpour_ipc::gen::kiapi;
use copperpour_sexp::writer::{read_consistent, write_atomic_if_unchanged};
use copperpour_sexp::SexpNode;
use prost::Message;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const TOOL: &str = "group_footprints";

pub(super) fn tool() -> ToolDef {
    tool!(
        "group_footprints",
        "Make footprints one KiCad board group, the group KiCad selects, moves and locks as a \
         block (what Ctrl+G does in the PCB editor). This is not group_components, which only \
         writes a 'Group' field on schematic symbols. With the board open in KiCad the group is \
         created live in one undo step; otherwise the closed board file gets it. Refused, with \
         nothing written, when a reference is not on the board, appears twice, or names a \
         footprint that is already in a group (ungroup it in KiCad first). The response \
         reports the group's uuid, name and members as read back after the write.",
        json!({
            "type": "object",
            "properties": {
                "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                "references": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string" },
                    "description": "Reference designators of the footprints to group"
                },
                "name": { "type": "string", "description": "Group name shown in KiCad" }
            },
            "required": ["board", "references", "name"]
        }),
        |args, ctx| async move { handle_group_footprints(args, ctx).await }
    )
    .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback)
}

/// The validated request: a group name and distinct, non-empty references in
/// the caller's order.
struct GroupRequest {
    name: String,
    references: Vec<String>,
}

impl GroupRequest {
    fn from_args(args: &serde_json::Value) -> Result<Self, CallToolResult> {
        let name = require_str(args, "name")?;
        if name.trim().is_empty() {
            return Err(invalid_arg("name", "must not be empty"));
        }
        let values = require_array(args, "references")?;
        if values.is_empty() {
            return Err(invalid_arg(
                "references",
                "must contain at least one reference",
            ));
        }
        let mut seen = BTreeSet::new();
        let mut references = Vec::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            let field = format!("references[{index}]");
            let Some(reference) = value.as_str().filter(|reference| !reference.is_empty()) else {
                return Err(invalid_arg(&field, "must be a non-empty string"));
            };
            if !seen.insert(reference) {
                return Err(invalid_arg(
                    &field,
                    &format!("duplicate footprint reference '{reference}'"),
                ));
            }
            references.push(reference.to_string());
        }
        Ok(Self {
            name: name.to_string(),
            references,
        })
    }

    /// The `references[i]` argument naming `reference`.
    fn field_of(&self, reference: &str) -> String {
        self.references
            .iter()
            .position(|requested| requested == reference)
            .map_or_else(|| "references".to_string(), |i| format!("references[{i}]"))
    }
}

/// One group member as read back: the footprint's reference and KIID.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Member {
    reference: String,
    uuid: String,
}

/// What the board says about the requested footprints before anything is
/// written, shared by the live and the file path so both refuse alike.
enum Resolution {
    /// Each request reference names exactly one footprint, none of them grouped.
    Ready(Vec<Member>),
    /// The request cannot be applied; nothing was written.
    Refused { field: String, reason: String },
}

/// Resolve `request` against the board's footprints (`(reference, uuid)` in
/// board order) and its groups' member lists (`uuid -> group label`).
fn resolve(
    request: &GroupRequest,
    footprints: &[(String, String)],
    grouped: &BTreeMap<String, String>,
) -> Resolution {
    let mut by_reference: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (reference, uuid) in footprints {
        by_reference
            .entry(reference.as_str())
            .or_default()
            .push(uuid.as_str());
    }
    let missing: Vec<&str> = request
        .references
        .iter()
        .map(String::as_str)
        .filter(|reference| !by_reference.contains_key(reference))
        .collect();
    if let Some(first) = missing.first() {
        return Resolution::Refused {
            field: request.field_of(first),
            reason: format!(
                "footprint{} {} not found on the board",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", ")
            ),
        };
    }
    let mut members = Vec::with_capacity(request.references.len());
    for reference in &request.references {
        let uuids = &by_reference[reference.as_str()];
        if uuids.len() > 1 {
            return Resolution::Refused {
                field: request.field_of(reference),
                reason: format!(
                    "footprint reference '{reference}' appears {} times on the board, so it \
                     does not name one footprint",
                    uuids.len()
                ),
            };
        }
        let uuid = uuids[0];
        if let Some(group) = grouped.get(uuid) {
            return Resolution::Refused {
                field: request.field_of(reference),
                reason: format!(
                    "footprint '{reference}' is already a member of {group}; KiCad keeps an \
                     item in one group only, so ungroup it in KiCad first"
                ),
            };
        }
        members.push(Member {
            reference: reference.clone(),
            uuid: uuid.to_string(),
        });
    }
    Resolution::Ready(members)
}

/// How a group is named in a refusal.
fn group_label(name: &str, uuid: &str) -> String {
    if name.is_empty() {
        format!("an unnamed group ({uuid})")
    } else {
        format!("group '{name}' ({uuid})")
    }
}

fn refused(field: String, reason: String) -> CallToolResult {
    let message =
        format!("The footprint group was not created: {reason}. The board was not modified.");
    CallToolResult::error_kind(ToolErrorKind::InvalidArgument { field, reason }, message)
}

/// The group as read back after the write.
struct Created {
    uuid: String,
    name: String,
    members: Vec<Member>,
}

impl Created {
    fn json(&self) -> serde_json::Value {
        json!({
            "uuid": self.uuid,
            "name": self.name,
            "members": self
                .members
                .iter()
                .map(|member| json!({ "reference": member.reference, "uuid": member.uuid }))
                .collect::<Vec<_>>()
        })
    }
}

/// What the live attempt observed.
enum Live {
    Created(Created),
    /// Refused before KiCad was asked to change anything.
    Refused {
        field: String,
        reason: String,
    },
    /// The commit was published, but the readback does not show the group
    /// that was asked for.
    Uncertain {
        uuid: String,
        reason: String,
    },
}

async fn handle_group_footprints(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let request = match GroupRequest::from_args(args) {
        Ok(request) => request,
        Err(error) => return Ok(error),
    };

    let live_request = GroupRequest {
        name: request.name.clone(),
        references: request.references.clone(),
    };
    match attempt_ipc_write_in(ctx, &board, "footprint group", move |client, document| {
        group_live(client, document, &live_request)
    })
    .await?
    {
        BoardWrite::Ipc(Live::Created(created)) => Ok(CallToolResult::json(&json!({
            "group": created.json(),
            "count": created.members.len(),
            "source": "ipc",
            "undo": "One KiCad undo step removes the group; the footprints stay where they are."
        }))),
        BoardWrite::Ipc(Live::Refused { field, reason }) => Ok(refused(field, reason)),
        BoardWrite::Ipc(Live::Uncertain { uuid, reason }) => Ok(CallToolResult::error_kind(
            ToolErrorKind::MutationOutcomeUncertain {
                operation: TOOL.to_string(),
                path: uuid.clone(),
                reason: reason.clone(),
            },
            format!(
                "KiCad accepted the commit that creates group {uuid}, but the readback does not \
                 confirm it: {reason}. Inspect the board in KiCad before retrying; this may not \
                 have been a no-op."
            ),
        )),
        BoardWrite::Refused(result) => Ok(result),
        BoardWrite::File(reason) => match group_closed_board(&board, &request) {
            Ok(FileOutcome::Created(created)) => Ok(CallToolResult::json(&json!({
                "group": created.json(),
                "count": created.members.len(),
                "source": "file",
                "fallback_reason": reason.evidence(),
                "warning": reason.warning()
            }))),
            Ok(FileOutcome::Refused { field, reason }) => Ok(refused(field, reason)),
            Err(error) => error.into_result(),
        },
    }
}

// ─── Live path ───────────────────────────────────────────────────────────────

fn group_live(
    client: &copperpour_ipc::client::KiCadIpcClient,
    document: kiapi::common::types::DocumentSpecifier,
    request: &GroupRequest,
) -> anyhow::Result<Live> {
    use kiapi::common::types::KiCadObjectType as Kot;

    let mut footprints = Vec::new();
    for item in client.get_items_in(document.clone(), Kot::KotPcbFootprint)? {
        if !copperpour_ipc::builders::any_is(&item, "kiapi.board.types.FootprintInstance") {
            continue;
        }
        let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())?;
        let reference = footprint
            .reference_field
            .as_ref()
            .and_then(|field| field.text.as_ref())
            .and_then(|text| text.text.as_ref())
            .map(|text| text.text.clone())
            .unwrap_or_default();
        let Some(id) = footprint.id.filter(|id| !id.value.is_empty()) else {
            anyhow::bail!("KiCad listed footprint '{reference}' without a KIID");
        };
        footprints.push((reference, id.value));
    }
    let grouped = live_group_members(client, document.clone())?;

    let members = match resolve(request, &footprints, &grouped) {
        Resolution::Ready(members) => members,
        Resolution::Refused { field, reason } => return Ok(Live::Refused { field, reason }),
    };

    let requested_uuid = uuid::Uuid::new_v4().to_string();
    let group = kiapi::board::types::Group {
        id: Some(kiapi::common::types::Kiid {
            value: requested_uuid.clone(),
        }),
        name: request.name.clone(),
        items: members
            .iter()
            .map(|member| kiapi::common::types::Kiid {
                value: member.uuid.clone(),
            })
            .collect(),
        ..Default::default()
    };
    let item = copperpour_ipc::builders::pack_any(&group, "kiapi.board.types.Group");
    let create_document = document.clone();
    let created = client.run_commit(&format!("Group footprints as {}", request.name), |c| {
        c.create_items_in_returning(create_document, vec![item])
    })?;

    // KiCad echoes the group it created; its KIID is the one to read back.
    // It keeps the one sent, but the echo is the authority when present.
    let uuid = created
        .iter()
        .filter(|item| copperpour_ipc::builders::any_is(item, "kiapi.board.types.Group"))
        .find_map(|item| kiapi::board::types::Group::decode(item.value.as_slice()).ok())
        .and_then(|group| group.id)
        .map(|id| id.value)
        .filter(|id| !id.is_empty())
        .unwrap_or(requested_uuid);

    Ok(
        match read_back_live(client, document, &uuid, request, &members) {
            Ok(created) => Live::Created(created),
            Err(reason) => Live::Uncertain {
                uuid,
                reason: format!("{reason:#}"),
            },
        },
    )
}

/// Every KIID that a board-level group lists as a member, with that group's
/// label. Nested groups list their members too, so a footprint in an inner
/// group is found.
fn live_group_members(
    client: &copperpour_ipc::client::KiCadIpcClient,
    document: kiapi::common::types::DocumentSpecifier,
) -> anyhow::Result<BTreeMap<String, String>> {
    use kiapi::common::types::KiCadObjectType as Kot;
    let mut grouped = BTreeMap::new();
    for item in client.get_items_in(document, Kot::KotPcbGroup)? {
        if !copperpour_ipc::builders::any_is(&item, "kiapi.board.types.Group") {
            continue;
        }
        let group = kiapi::board::types::Group::decode(item.value.as_slice())?;
        let uuid = group.id.map(|id| id.value).unwrap_or_default();
        let label = group_label(&group.name, &uuid);
        for member in group.items {
            grouped.insert(member.value, label.clone());
        }
    }
    Ok(grouped)
}

/// The group `uuid` as KiCad holds it, checked against the request: the name
/// asked for and exactly the footprints resolved for it.
fn read_back_live(
    client: &copperpour_ipc::client::KiCadIpcClient,
    document: kiapi::common::types::DocumentSpecifier,
    uuid: &str,
    request: &GroupRequest,
    members: &[Member],
) -> anyhow::Result<Created> {
    let items = client.get_items_by_id_in(document, &[uuid.to_string()])?;
    let group = items
        .iter()
        .filter(|item| copperpour_ipc::builders::any_is(item, "kiapi.board.types.Group"))
        .find_map(|item| kiapi::board::types::Group::decode(item.value.as_slice()).ok())
        .filter(|group| group.id.as_ref().is_some_and(|id| id.value == uuid))
        .ok_or_else(|| anyhow::anyhow!("KiCad holds no group {uuid} after the commit"))?;
    let held: BTreeSet<String> = group.items.into_iter().map(|id| id.value).collect();
    check_members(uuid, &group.name, &held, request, members)
}

/// Compare a group read back after a write with what was asked for. Both
/// paths use it, so a member KiCad skipped or a renamed group reads the same.
fn check_members(
    uuid: &str,
    name: &str,
    held: &BTreeSet<String>,
    request: &GroupRequest,
    members: &[Member],
) -> anyhow::Result<Created> {
    if name != request.name {
        anyhow::bail!(
            "group {uuid} is named '{name}', not '{}' as asked",
            request.name
        );
    }
    let wanted: BTreeSet<String> = members.iter().map(|member| member.uuid.clone()).collect();
    if *held != wanted {
        let absent: Vec<&str> = members
            .iter()
            .filter(|member| !held.contains(&member.uuid))
            .map(|member| member.reference.as_str())
            .collect();
        let extra = held.difference(&wanted).count();
        anyhow::bail!(
            "group {uuid} does not hold exactly the requested footprints (missing: {}; \
             {extra} other member{})",
            if absent.is_empty() {
                "none".to_string()
            } else {
                absent.join(", ")
            },
            if extra == 1 { "" } else { "s" }
        );
    }
    Ok(Created {
        uuid: uuid.to_string(),
        name: name.to_string(),
        members: members.to_vec(),
    })
}

// ─── Closed-board file path ──────────────────────────────────────────────────

enum FileOutcome {
    Created(Created),
    Refused { field: String, reason: String },
}

fn group_closed_board(
    board_path: &Path,
    request: &GroupRequest,
) -> Result<FileOutcome, ClosedBoardError> {
    let content = read_consistent(board_path).map_err(|e| ClosedBoardError::Io(e.into()))?;
    check_single_board_form(&content).map_err(ClosedBoardError::Unusable)?;
    let root = copperpour_sexp::parse_sexp(&content)
        .map_err(|e| ClosedBoardError::Unusable(format!("the board does not parse: {e}")))?;
    if root.head() != Some("kicad_pcb") {
        return Err(ClosedBoardError::Unusable(
            "the file is not a kicad_pcb board".to_string(),
        ));
    }

    let mut footprints = Vec::new();
    for footprint in root.find_all("footprint") {
        let reference = footprint_reference(footprint).unwrap_or_default();
        let Some(uuid) = node_uuid(footprint) else {
            return Err(ClosedBoardError::Unusable(format!(
                "footprint '{reference}' has no (uuid …); open and save the board in KiCad 10 \
                 first"
            )));
        };
        footprints.push((reference, uuid));
    }
    let grouped = file_group_members(&root);

    let members = match resolve(request, &footprints, &grouped) {
        Resolution::Ready(members) => members,
        Resolution::Refused { field, reason } => return Ok(FileOutcome::Refused { field, reason }),
    };

    let uuid = uuid::Uuid::new_v4().to_string();
    let updated = insert_group_block(&content, &uuid, &request.name, &members);
    check_single_board_form(&updated).map_err(ClosedBoardError::Unusable)?;
    write_atomic_if_unchanged(board_path, &content, &updated)
        .map_err(|e| ClosedBoardError::Io(e.into()))?;

    // The writer has confirmed the file holds `updated`; read the group out
    // of that text, not out of the request.
    let written = copperpour_sexp::parse_sexp(&updated).map_err(|e| {
        ClosedBoardError::Unusable(format!("the written board does not parse: {e}"))
    })?;
    let group = written
        .find_all("group")
        .into_iter()
        .find(|group| node_uuid(group).as_deref() == Some(uuid.as_str()))
        .ok_or_else(|| {
            ClosedBoardError::Unusable(format!("group {uuid} is missing from the written board"))
        })?;
    let name = group.get(1).and_then(SexpNode::as_str).unwrap_or_default();
    let held: BTreeSet<String> = group_member_ids(group).into_iter().collect();
    check_members(&uuid, name, &held, request, &members)
        .map(FileOutcome::Created)
        .map_err(|e| ClosedBoardError::Unusable(format!("{e:#}")))
}

/// The `(uuid "…")` of a board item.
fn node_uuid(node: &SexpNode) -> Option<String> {
    node.find("uuid")
        .and_then(|uuid| uuid.get(1))
        .and_then(SexpNode::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
}

/// The KIIDs one `(group …)` lists under `(members …)`.
fn group_member_ids(group: &SexpNode) -> Vec<String> {
    group
        .find("members")
        .and_then(SexpNode::children)
        .map(|children| {
            children
                .iter()
                .skip(1)
                .filter_map(SexpNode::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// As [`live_group_members`], from the board-level `(group …)` entries of a
/// board file. Groups inside a footprint group that footprint's own drawings
/// and are not board-level, so they are not read.
fn file_group_members(root: &SexpNode) -> BTreeMap<String, String> {
    let mut grouped = BTreeMap::new();
    for group in root.find_all("group") {
        let name = group.get(1).and_then(SexpNode::as_str).unwrap_or_default();
        let label = group_label(name, &node_uuid(group).unwrap_or_default());
        for member in group_member_ids(group) {
            grouped.insert(member, label.clone());
        }
    }
    grouped
}

/// `content` with a board-level `(group …)` for `members` added as the last
/// item of the board, in the shape pcbnew 10 writes it.
fn insert_group_block(content: &str, uuid: &str, name: &str, members: &[Member]) -> String {
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let member_ids: Vec<String> = members
        .iter()
        .map(|member| copperpour_sexp::title_block::quote(&member.uuid))
        .collect();
    let block = format!(
        "\t(group {name}{eol}\t\t(uuid {uuid}){eol}\t\t(members {members}){eol}\t)",
        name = copperpour_sexp::title_block::quote(name),
        uuid = copperpour_sexp::title_block::quote(uuid),
        members = member_ids.join(" "),
    );
    let close = content.rfind(')').unwrap_or(content.len());
    let line_start = content[..close]
        .rfind('\n')
        .map_or(close, |index| index + 1);
    if content[line_start..close].trim().is_empty() {
        // The board's closing parenthesis has a line of its own, as pcbnew
        // writes it: the group goes on the lines before it.
        format!(
            "{}{block}{eol}{}",
            &content[..line_start],
            &content[line_start..]
        )
    } else {
        format!(
            "{}{eol}{block}{eol}{}",
            &content[..close],
            &content[close..]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// A copy of a KiCad-authored fixture in a fresh directory.
    fn board_copy(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join(name);
        std::fs::copy(fixture(name), &board).unwrap();
        (dir, board)
    }

    fn ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                // No IPC address: the gate finds no live KiCad and the file
                // path runs.
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn text(result: &CallToolResult) -> String {
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    async fn group(board: &Path, references: &[&str], name: &str) -> CallToolResult {
        handle_group_footprints(
            &json!({
                "board": board.to_string_lossy(),
                "references": references,
                "name": name,
            }),
            &ctx(),
        )
        .await
        .unwrap()
    }

    /// The board-level groups of a board file as (name, uuid, members).
    fn board_groups(board: &Path) -> Vec<(String, String, Vec<String>)> {
        let content = std::fs::read_to_string(board).unwrap();
        let root = copperpour_sexp::parse_sexp(&content).unwrap();
        root.find_all("group")
            .into_iter()
            .map(|group| {
                (
                    group.get(1).and_then(SexpNode::as_str).unwrap().to_string(),
                    node_uuid(group).unwrap(),
                    group_member_ids(group),
                )
            })
            .collect()
    }

    // board_source_divergence_kicad10: R1 = 1111…, R2 = 2222…, no groups.
    const R1: &str = "11111111-1111-4111-8111-111111111111";
    const R2: &str = "22222222-2222-4222-8222-222222222222";

    #[tokio::test]
    async fn closed_board_gets_a_board_group_of_the_footprints() {
        let (_dir, board) = board_copy("board_source_divergence_kicad10.kicad_pcb");
        let result = group(&board, &["R2", "R1"], "Senken \"A\"").await;
        assert!(!result.is_error, "{}", text(&result));
        let response: serde_json::Value = serde_json::from_str(&text(&result)).unwrap();

        // Independent of the writer: parse the file and find the group there.
        let groups = board_groups(&board);
        assert_eq!(groups.len(), 1, "{groups:?}");
        let (name, uuid, members) = &groups[0];
        assert_eq!(name, "Senken \"A\"");
        assert_eq!(
            members.iter().collect::<BTreeSet<_>>(),
            [R1.to_string(), R2.to_string()].iter().collect()
        );

        assert_eq!(response["source"], "file");
        assert_eq!(response["count"], 2);
        assert_eq!(response["group"]["uuid"], json!(uuid));
        assert_eq!(response["group"]["name"], "Senken \"A\"");
        assert_eq!(response["group"]["members"][0]["reference"], "R2");
        assert_eq!(response["group"]["members"][0]["uuid"], R2);
        assert_eq!(response["fallback_reason"]["kind"], "transport_unreachable");

        // pcbnew's shape: the group sits before the board's closing line.
        let written = std::fs::read_to_string(&board).unwrap();
        assert!(written.ends_with("\t)\n)\n"), "{written}");
        check_single_board_form(&written).unwrap();
    }

    #[tokio::test]
    async fn closed_board_refuses_a_footprint_already_in_a_group() {
        // KiCad-authored: U1 and R1 form a board-level group, and U1 owns a
        // group of its own silkscreen lines inside the footprint.
        let (_dir, board) = board_copy("footprint_children_kicad10.kicad_pcb");
        let before = std::fs::read_to_string(&board).unwrap();

        let result = group(&board, &["R1"], "Neu").await;
        assert!(result.is_error);
        let message = text(&result);
        assert!(message.contains("invalid_argument"), "{message}");
        assert!(message.contains("references[0]"), "{message}");
        assert!(
            message.contains("b59c648d-9112-4abe-949f-e417d0581ee7"),
            "{message}"
        );
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn closed_board_refuses_missing_and_repeated_references_without_writing() {
        let (_dir, board) = board_copy("board_source_divergence_kicad10.kicad_pcb");
        let before = std::fs::read_to_string(&board).unwrap();

        let missing = group(&board, &["R1", "R404"], "G").await;
        assert!(missing.is_error);
        let message = text(&missing);
        assert!(
            message.contains("references[1]") && message.contains("R404"),
            "{message}"
        );

        let repeated = group(&board, &["R1", "R1"], "G").await;
        assert!(repeated.is_error);
        assert!(text(&repeated).contains("references[1]"));

        let unnamed = group(&board, &["R1"], " ").await;
        assert!(unnamed.is_error);
        assert!(text(&unnamed).contains("name"));

        assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn served_dispatch_rejects_an_empty_reference_list_before_writing() {
        let (_dir, board) = board_copy("board_source_divergence_kicad10.kicad_pcb");
        let before = std::fs::read_to_string(&board).unwrap();
        let handler = crate::mcp::handler::McpHandler::new(ServerConfig {
            eager_toolsets: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "group_footprints",
                    "arguments": {
                        "board": board.to_string_lossy(),
                        "references": [],
                        "name": "G"
                    }
                }
            }))
            .await
            .unwrap();
        let result = response.result.unwrap();
        assert_eq!(result["isError"], json!(true), "{result}");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
    }

    // ─── Live path against a KiCad double ────────────────────────────────────

    mod live {
        use super::*;
        use crate::tools::pcb_board::board_mock::spawn_kicad_holding_board;
        use copperpour_ipc::builders::{any_is, pack_any};
        use kiapi::common::commands;
        use kiapi::common::types::KiCadObjectType as Kot;
        use std::sync::{Arc, Mutex};

        /// What the double holds and was asked.
        #[derive(Default)]
        struct Held {
            footprints: Vec<prost_types::Any>,
            groups: Vec<kiapi::board::types::Group>,
            creates: usize,
            commits: Vec<String>,
            /// Drop this member when storing a created group, as KiCad does
            /// for a KIID it cannot resolve.
            skip_member: Option<String>,
        }

        fn footprint(reference: &str, uuid: &str) -> prost_types::Any {
            use kiapi::board::types as board;
            pack_any(
                &board::FootprintInstance {
                    id: Some(kiapi::common::types::Kiid {
                        value: uuid.to_string(),
                    }),
                    reference_field: Some(board::Field {
                        text: Some(board::BoardText {
                            text: Some(kiapi::common::types::Text {
                                text: reference.to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                "kiapi.board.types.FootprintInstance",
            )
        }

        fn items_response(items: Vec<prost_types::Any>) -> prost_types::Any {
            pack_any(
                &commands::GetItemsResponse {
                    header: None,
                    status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                    items,
                },
                "kiapi.common.commands.GetItemsResponse",
            )
        }

        fn spawn(board: &Path, held: Arc<Mutex<Held>>) -> crate::test_support::MockIpcServer {
            spawn_kicad_holding_board(board, move |command| {
                let mut held = held.lock().unwrap();
                let url = command.type_url.as_str();
                if url.ends_with("BeginCommit") {
                    return Some(pack_any(
                        &commands::BeginCommitResponse {
                            id: Some(kiapi::common::types::Kiid {
                                value: "commit".to_string(),
                            }),
                        },
                        "kiapi.common.commands.BeginCommitResponse",
                    ));
                }
                if url.ends_with("EndCommit") {
                    let end = commands::EndCommit::decode(command.value.as_slice()).unwrap();
                    held.commits.push(end.message);
                    return Some(pack_any(
                        &commands::EndCommitResponse {},
                        "kiapi.common.commands.EndCommitResponse",
                    ));
                }
                if url.ends_with("GetItemsById") {
                    let request = commands::GetItemsById::decode(command.value.as_slice()).unwrap();
                    let items = held
                        .groups
                        .iter()
                        .filter(|group| {
                            request.items.iter().any(|id| group.id.as_ref() == Some(id))
                        })
                        .map(|group| pack_any(group, "kiapi.board.types.Group"))
                        .collect();
                    return Some(items_response(items));
                }
                if url.ends_with("GetItems") {
                    let request = commands::GetItems::decode(command.value.as_slice()).unwrap();
                    let mut items = Vec::new();
                    if request.types.contains(&(Kot::KotPcbFootprint as i32)) {
                        items.extend(held.footprints.iter().cloned());
                    }
                    if request.types.contains(&(Kot::KotPcbGroup as i32)) {
                        items.extend(
                            held.groups
                                .iter()
                                .map(|group| pack_any(group, "kiapi.board.types.Group")),
                        );
                    }
                    return Some(items_response(items));
                }
                if url.ends_with("CreateItems") {
                    let request = commands::CreateItems::decode(command.value.as_slice()).unwrap();
                    held.creates += 1;
                    let mut created = Vec::new();
                    for item in request.items {
                        assert!(any_is(&item, "kiapi.board.types.Group"));
                        let mut group =
                            kiapi::board::types::Group::decode(item.value.as_slice()).unwrap();
                        if let Some(skip) = held.skip_member.clone() {
                            group.items.retain(|id| id.value != skip);
                        }
                        held.groups.push(group.clone());
                        created.push(commands::ItemCreationResult {
                            status: Some(commands::ItemStatus {
                                code: commands::ItemStatusCode::IscOk as i32,
                                error_message: String::new(),
                            }),
                            item: Some(pack_any(&group, "kiapi.board.types.Group")),
                        });
                    }
                    return Some(pack_any(
                        &commands::CreateItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            created_items: created,
                        },
                        "kiapi.common.commands.CreateItemsResponse",
                    ));
                }
                None
            })
        }

        async fn call(
            board: &Path,
            held: Arc<Mutex<Held>>,
            references: &[&str],
        ) -> serde_json::Value {
            let server = spawn(board, held);
            let handler = crate::mcp::handler::McpHandler::new(ServerConfig {
                ipc_address: server.address().to_string(),
                eager_toolsets: true,
                ..Default::default()
            })
            .await
            .unwrap();
            let response = handler
                .handle_message(json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {
                        "name": "group_footprints",
                        "arguments": {
                            "board": board.to_string_lossy(),
                            "references": references,
                            "name": "HV"
                        }
                    }
                }))
                .await
                .unwrap();
            response.result.unwrap()
        }

        fn live_board() -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let board = dir.path().join("live.kicad_pcb");
            std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
            (dir, board)
        }

        fn held_with(footprints: &[(&str, &str)]) -> Arc<Mutex<Held>> {
            Arc::new(Mutex::new(Held {
                footprints: footprints
                    .iter()
                    .map(|(reference, uuid)| footprint(reference, uuid))
                    .collect(),
                ..Default::default()
            }))
        }

        #[tokio::test]
        async fn live_board_gets_one_group_in_one_commit() {
            let (_dir, board) = live_board();
            let before = std::fs::read_to_string(&board).unwrap();
            let held = held_with(&[("Q1", "q1"), ("R5", "r5"), ("C3", "c3")]);
            let result = call(&board, held.clone(), &["Q1", "C3"]).await;
            assert_eq!(result["isError"], json!(false), "{result}");
            let response: serde_json::Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();

            let held = held.lock().unwrap();
            assert_eq!(held.creates, 1);
            assert_eq!(held.commits, vec!["Group footprints as HV".to_string()]);
            assert_eq!(held.groups.len(), 1);
            let group = &held.groups[0];
            assert_eq!(group.name, "HV");
            let ids: Vec<&str> = group.items.iter().map(|id| id.value.as_str()).collect();
            assert_eq!(ids, ["q1", "c3"]);

            assert_eq!(response["source"], "ipc");
            assert_eq!(response["count"], 2);
            assert_eq!(
                response["group"]["uuid"],
                json!(group.id.as_ref().unwrap().value)
            );
            assert_eq!(response["group"]["members"][1]["reference"], "C3");
            // The live path never touches the file.
            assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
        }

        #[tokio::test]
        async fn live_board_refuses_a_grouped_footprint_before_any_commit() {
            let (_dir, board) = live_board();
            let held = held_with(&[("Q1", "q1"), ("R5", "r5")]);
            held.lock()
                .unwrap()
                .groups
                .push(kiapi::board::types::Group {
                    id: Some(kiapi::common::types::Kiid {
                        value: "old".to_string(),
                    }),
                    name: "Ausgang".to_string(),
                    items: vec![kiapi::common::types::Kiid {
                        value: "r5".to_string(),
                    }],
                    ..Default::default()
                });
            let result = call(&board, held.clone(), &["Q1", "R5"]).await;
            assert_eq!(result["isError"], json!(true), "{result}");
            let message = result["content"][0]["text"].as_str().unwrap();
            assert!(message.contains("references[1]"), "{message}");
            assert!(message.contains("group 'Ausgang' (old)"), "{message}");
            let held = held.lock().unwrap();
            assert_eq!(held.creates, 0);
            assert!(held.commits.is_empty());
            assert_eq!(held.groups.len(), 1);
        }

        #[tokio::test]
        async fn live_board_refuses_a_missing_reference_before_any_commit() {
            let (_dir, board) = live_board();
            let held = held_with(&[("Q1", "q1")]);
            let result = call(&board, held.clone(), &["Q1", "Q9"]).await;
            assert_eq!(result["isError"], json!(true), "{result}");
            let message = result["content"][0]["text"].as_str().unwrap();
            assert!(
                message.contains("references[1]") && message.contains("Q9"),
                "{message}"
            );
            assert_eq!(held.lock().unwrap().creates, 0);
        }

        #[tokio::test]
        async fn a_member_kicad_dropped_makes_the_outcome_uncertain() {
            let (_dir, board) = live_board();
            let held = held_with(&[("Q1", "q1"), ("R5", "r5")]);
            held.lock().unwrap().skip_member = Some("r5".to_string());
            let result = call(&board, held.clone(), &["Q1", "R5"]).await;
            assert_eq!(result["isError"], json!(true), "{result}");
            let message = result["content"][0]["text"].as_str().unwrap();
            assert!(message.contains("mutation_outcome_uncertain"), "{message}");
            assert!(message.contains("missing: R5"), "{message}");
        }
    }
}
