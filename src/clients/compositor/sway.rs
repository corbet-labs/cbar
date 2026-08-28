#[cfg(feature = "workspaces+sway")]
use super::{Visibility, Workspace};
#[cfg(any(
    feature = "workspaces+sway",
    feature = "keyboard+sway",
    feature = "bindmode+sway"
))]
use crate::await_sync;
#[cfg(any(
    feature = "workspaces+sway",
    feature = "keyboard+sway",
    feature = "bindmode+sway"
))]
use crate::clients::sway::Client;
#[cfg(feature = "bindmode+sway")]
use crate::clients::sway::ModeEvent;
#[cfg(feature = "keyboard+sway")]
use crate::clients::sway::{InputChange, InputEvent};
#[cfg(feature = "workspaces+sway")]
use crate::clients::sway::{Node, Workspace as SwayWorkspace, WorkspaceChange, WorkspaceEvent};
#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
use crate::{error, spawn};
#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
use color_eyre::Report;
#[cfg(any(
    feature = "workspaces+sway",
    feature = "keyboard+sway",
    feature = "bindmode+sway"
))]
use tokio::sync::broadcast::{Receiver, channel};

#[cfg(feature = "workspaces+sway")]
use super::{WorkspaceTarget, WorkspaceUpdate};

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn quote_sway_argument(value: &str) -> Result<String, Report> {
    if value
        .chars()
        .any(|character| matches!(character, '\0' | '\n' | '\r'))
    {
        return Err(Report::msg(
            "Sway command arguments cannot contain NUL or line breaks",
        ));
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '$' => quoted.push_str("$$"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

#[cfg(any(feature = "workspaces+sway", feature = "keyboard+sway"))]
fn next_keyboard_layout_command(identifier: &str) -> Result<String, Report> {
    Ok(format!(
        "input {} xkb_switch_layout next",
        quote_sway_argument(identifier)?
    ))
}

#[cfg(feature = "workspaces+sway")]
fn focus_command_for_target(
    target: &WorkspaceTarget,
    workspace: Option<&SwayWorkspace>,
) -> Result<String, Report> {
    match target.persistent_by_name() {
        WorkspaceTarget::Name(name) => Ok(format!("workspace {}", quote_sway_argument(&name)?)),
        WorkspaceTarget::Id(id) => {
            let workspace = workspace
                .ok_or_else(|| Report::msg(format!("couldn't find workspace with id {id}")))?;

            // Numbered workspaces are focused by number rather than name.
            // The name can be changed by another process (e.g. a
            // workspace renamer reacting to the focus-change this same
            // command causes) in the gap between the query above and the
            // command below, which previously made sway create a new,
            // empty workspace instead of focusing the existing one. The
            // number is stable across such renames, so prefer it when
            // available; sway reports -1 for workspaces with no number.
            if workspace.num >= 0 {
                Ok(format!("workspace number {}", workspace.num))
            } else {
                Ok(format!(
                    "workspace {}",
                    quote_sway_argument(&workspace.name)?
                ))
            }
        }
        WorkspaceTarget::Persistent { .. } => unreachable!("persistent target was resolved"),
    }
}

#[cfg(feature = "workspaces+sway")]
impl super::WorkspaceClient for Client {
    fn focus(&self, target: WorkspaceTarget) {
        let client = self.connection().clone();
        spawn(async move {
            let mut client = client.lock().await;

            let workspace = if let WorkspaceTarget::Id(id) = target.persistent_by_name() {
                client
                    .get_workspaces()
                    .await?
                    .into_iter()
                    .find(|workspace| workspace.id == id)
            } else {
                None
            };

            let command = focus_command_for_target(&target, workspace.as_ref())?;

            if let Err(e) = client.run_command(command).await {
                return Err(Report::msg(format!(
                    "Couldn't focus workspace '{target:?}': {e:#}"
                )));
            }

            Ok(())
        });
    }

    fn subscribe(&self) -> Receiver<WorkspaceUpdate> {
        let (tx, rx) = channel(16);

        let client = self.connection().clone();

        // TODO: this needs refactoring
        await_sync(async {
            let mut client = client.lock().await;
            match client.get_workspaces().await {
                Ok(workspaces) => {
                    let event = WorkspaceUpdate::Init(
                        workspaces.into_iter().map(Workspace::from).collect(),
                    );
                    let _ = tx.send(event);
                }
                Err(error) => {
                    error!("Failed to get initial Sway workspaces: {error:#}");
                    let _ = tx.send(WorkspaceUpdate::Init(Vec::new()));
                }
            }

            drop(client);

            if let Err(error) = self
                .add_workspace_listener(move |event| {
                    let update = WorkspaceUpdate::from(event.clone());
                    let _ = tx.send(update);
                })
                .await
            {
                error!("Failed to register Sway workspace listener: {error:#}");
            }
        });

        rx
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<Node> for Workspace {
    fn from(node: Node) -> Self {
        let visibility = Visibility::from(&node);

        Self {
            id: node.id,
            index: node.num.unwrap_or(0),
            name: node.name.unwrap_or_default(),
            monitor: node.output.unwrap_or_default(),
            visibility,
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<SwayWorkspace> for Workspace {
    fn from(workspace: SwayWorkspace) -> Self {
        let visibility = Visibility::from(&workspace);

        Self {
            id: workspace.id,
            index: workspace.num,
            name: workspace.name,
            monitor: workspace.output,
            visibility,
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<&Node> for Visibility {
    fn from(node: &Node) -> Self {
        if node.focused {
            Self::focused()
        } else if node.visible.unwrap_or(false) {
            Self::visible()
        } else {
            Self::Hidden
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<&SwayWorkspace> for Visibility {
    fn from(workspace: &SwayWorkspace) -> Self {
        if workspace.focused {
            Self::focused()
        } else if workspace.visible {
            Self::visible()
        } else {
            Self::Hidden
        }
    }
}

#[cfg(feature = "workspaces+sway")]
impl From<WorkspaceEvent> for WorkspaceUpdate {
    fn from(event: WorkspaceEvent) -> Self {
        match event.change {
            WorkspaceChange::Init => event
                .current
                .map(Workspace::from)
                .map(Self::Add)
                .unwrap_or(Self::Unknown),
            WorkspaceChange::Empty => event
                .current
                .or(event.old)
                .map(|workspace| Self::Remove(workspace.id))
                .unwrap_or(Self::Unknown),
            WorkspaceChange::Focus => event.current.map_or(Self::Unknown, |current| Self::Focus {
                old: event.old.map(Workspace::from),
                new: Workspace::from(current),
            }),
            WorkspaceChange::Move => event
                .current
                .map(Workspace::from)
                .map(Self::Move)
                .unwrap_or(Self::Unknown),
            WorkspaceChange::Rename => {
                if let Some(node) = event.current {
                    Self::Rename {
                        id: node.id,
                        name: node.name.unwrap_or_default(),
                    }
                } else {
                    Self::Unknown
                }
            }
            WorkspaceChange::Urgent => {
                if let Some(node) = event.current {
                    Self::Urgent {
                        id: node.id,
                        urgent: node.urgent,
                    }
                } else {
                    Self::Unknown
                }
            }
            _ => Self::Unknown,
        }
    }
}

#[cfg(all(test, feature = "workspaces+sway"))]
mod tests {
    use super::*;

    #[test]
    fn closed_numbered_favourite_focuses_by_name() {
        let target = WorkspaceTarget::Persistent {
            name: "2".to_string(),
            index: Some(2),
        };

        assert_eq!(
            focus_command_for_target(&target, None).expect("named target to produce a command"),
            "workspace \"2\""
        );
    }

    #[test]
    fn open_numbered_workspace_keeps_stable_number_command() {
        let target = WorkspaceTarget::Id(91);
        let workspace = SwayWorkspace {
            id: 91,
            num: 8,
            name: "renamed meanwhile".to_string(),
            output: "DP-1".to_string(),
            visible: false,
            focused: false,
        };
        assert_eq!(
            focus_command_for_target(&target, Some(&workspace))
                .expect("numbered workspace to produce a command"),
            "workspace number 8"
        );
    }

    #[test]
    fn workspace_and_input_arguments_are_quoted_against_command_injection() {
        assert_eq!(
            quote_sway_argument("name\"; exec danger \\\\ tail")
                .expect("injection-shaped name should be quoted"),
            "\"name\\\"; exec danger \\\\\\\\ tail\""
        );
        assert_eq!(
            quote_sway_argument("$mod").expect("variable-shaped name should stay literal"),
            "\"$$mod\""
        );
        assert!(quote_sway_argument("bad\nexec danger").is_err());
        assert!(quote_sway_argument("bad\0tail").is_err());

        let target = WorkspaceTarget::Name("x; exec danger".to_string());
        assert_eq!(
            focus_command_for_target(&target, None)
                .expect("injection-shaped target should remain one argument"),
            "workspace \"x; exec danger\""
        );
        assert_eq!(
            next_keyboard_layout_command("kbd\"; exec danger")
                .expect("injection-shaped identifier should remain one argument"),
            "input \"kbd\\\"; exec danger\" xkb_switch_layout next"
        );
    }

    #[test]
    fn incomplete_workspace_events_degrade_without_panicking() {
        let missing = WorkspaceUpdate::from(WorkspaceEvent {
            change: WorkspaceChange::Focus,
            current: None,
            old: None,
        });
        assert!(matches!(missing, WorkspaceUpdate::Unknown));

        let removed = WorkspaceUpdate::from(WorkspaceEvent {
            change: WorkspaceChange::Empty,
            current: None,
            old: Some(Node {
                id: 44,
                num: Some(4),
                name: Some("4".to_string()),
                output: Some("DP-1".to_string()),
                visible: Some(false),
                focused: false,
                urgent: false,
            }),
        });
        assert!(matches!(removed, WorkspaceUpdate::Remove(44)));
    }
}

#[cfg(feature = "keyboard+sway")]
use super::{KeyboardLayoutClient, KeyboardLayoutUpdate};

#[cfg(feature = "keyboard+sway")]
impl KeyboardLayoutClient for Client {
    fn set_next_active(&self) {
        let client = self.connection().clone();
        spawn(async move {
            let mut client = client.lock().await;

            let inputs = match client.get_inputs().await {
                Ok(inputs) => inputs,
                Err(error) => {
                    error!("Failed to get Sway inputs: {error:#}");
                    return;
                }
            };

            if let Some(keyboard) = inputs
                .into_iter()
                .find(|i| i.xkb_active_layout_name.is_some())
            {
                let command = match next_keyboard_layout_command(&keyboard.identifier) {
                    Ok(command) => command,
                    Err(error) => {
                        error!("Failed to build Sway keyboard command: {error:#}");
                        return;
                    }
                };
                if let Err(e) = client.run_command(command).await {
                    error!("Failed to switch keyboard layout due to Sway error: {e}");
                }
            } else {
                error!("Failed to get keyboard identifier from Sway");
            }
        });
    }

    fn subscribe(&self) -> Receiver<KeyboardLayoutUpdate> {
        let (tx, rx) = channel(16);

        let client = self.connection().clone();

        await_sync(async {
            let mut client = client.lock().await;
            match client.get_inputs().await {
                Ok(inputs) => {
                    if let Some(layout) = inputs.into_iter().find_map(|i| i.xkb_active_layout_name)
                    {
                        let _ = tx.send(KeyboardLayoutUpdate(layout));
                    } else {
                        error!("Failed to get keyboard layout from Sway!");
                    }
                }
                Err(error) => error!("Failed to get initial Sway keyboard layout: {error:#}"),
            }

            drop(client);

            if let Err(error) = self
                .add_input_listener(move |event| {
                    if let Ok(layout) = KeyboardLayoutUpdate::try_from(event.clone()) {
                        let _ = tx.send(layout);
                    }
                })
                .await
            {
                error!("Failed to register Sway input listener: {error:#}");
            }
        });

        rx
    }
}

#[cfg(feature = "keyboard+sway")]
impl TryFrom<InputEvent> for KeyboardLayoutUpdate {
    type Error = ();

    fn try_from(value: InputEvent) -> Result<Self, Self::Error> {
        match value.change {
            InputChange::XkbLayout | InputChange::XkbKeymap => {
                if let Some(layout) = value.input.xkb_active_layout_name {
                    Ok(KeyboardLayoutUpdate(layout))
                } else {
                    Err(())
                }
            }
            _ => Err(()),
        }
    }
}

#[cfg(feature = "bindmode+sway")]
use super::{BindModeClient, BindModeUpdate};

#[cfg(feature = "bindmode+sway")]
impl BindModeClient for Client {
    fn subscribe(&self) -> super::Result<Receiver<BindModeUpdate>> {
        let (tx, rx) = channel(16);

        await_sync(async {
            self.add_mode_listener(move |mode: &ModeEvent| {
                tracing::trace!("mode: {:?}", mode);

                // when no binding is active the bindmode is named "default", but we must display
                // nothing in this case.
                let name = if mode.change == "default" {
                    String::new()
                } else {
                    mode.change.clone()
                };

                let _ = tx.send(BindModeUpdate {
                    name,
                    pango_markup: mode.pango_markup,
                });
            })
            .await
        })
        .map_err(|err| super::Error::Other(err.into()))?;

        Ok(rx)
    }
}
