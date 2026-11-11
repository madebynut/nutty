//! A split tree with stable pane identity and deterministic collapse behavior.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod session;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    Pane(Uuid),
    Split {
        id: Uuid,
        axis: Axis,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    pub fn id(&self) -> Uuid {
        match self {
            Self::Pane(id) | Self::Split { id, .. } => *id,
        }
    }
    pub fn split(&mut self, target: Uuid, new: Uuid, axis: Axis) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    id: Uuid::new_v4(),
                    axis,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, axis) || second.split(target, new, axis)
            }
        }
    }

    pub fn remove(self, target: Uuid) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != target).then_some(Self::Pane(id)),
            Self::Split {
                id,
                axis,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    id,
                    axis,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            },
        }
    }

    pub fn panes(&self) -> Vec<Uuid> {
        match self {
            Self::Pane(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut ids = first.panes();
                ids.extend(second.panes());
                ids
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_split_collapses_only_the_closed_branch() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let mut layout = Layout::Pane(a);
        layout.split(a, b, Axis::Horizontal);
        layout.split(b, c, Axis::Vertical);
        let split_id = match layout {
            Layout::Split { id, .. } => id,
            _ => panic!("root must be a split"),
        };
        assert_eq!(layout.panes(), vec![a, b, c]);
        let layout = layout.remove(b).expect("two panes survive");
        assert_eq!(
            layout,
            Layout::Split {
                id: split_id,
                axis: Axis::Horizontal,
                first: Box::new(Layout::Pane(a)),
                second: Box::new(Layout::Pane(c))
            }
        );
        assert_eq!(layout.remove(a), Some(Layout::Pane(c)));
    }
}
