//! The Commits view's history graph, laid out as VS Code lays it out: going
//! down the commits (newest first, in topological order), each row's lanes
//! at the bottom are its lanes at the top with the commit's lane handed to
//! its first parent, plus a lane for each other parent that has none. The
//! editor draws them (`views::set-graph`).

use flareite_extension_api::views::{GraphLane, GraphRow};

use crate::history::Commit;

/// Colors lanes take by turns.
const COLORS: u32 = 5;

pub fn lay_out(commits: &[Commit]) -> Vec<GraphRow> {
  let mut lanes: Vec<GraphLane> = Vec::new();
  let mut next_color = 0;
  let mut color = || {
    let color = next_color % COLORS;
    next_color += 1;
    color
  };
  commits
    .iter()
    .map(|commit| {
      let inputs = lanes.clone();
      let mut outputs: Vec<GraphLane> = Vec::new();
      // The commit's first lane goes on to its first parent; other lanes
      // for it end here.
      let mut continued = false;
      for lane in &inputs {
        if lane.id != commit.hash {
          outputs.push(lane.clone());
        } else if !continued {
          continued = true;
          if let Some(first) = commit.parents.first() {
            outputs.push(GraphLane {
              id: first.clone(),
              color: lane.color,
            });
          }
        }
      }
      for (index, parent) in commit.parents.iter().enumerate() {
        if index == 0 && continued {
          continue;
        }
        if !outputs.iter().any(|lane| lane.id == *parent) {
          outputs.push(GraphLane {
            id: parent.clone(),
            color: color(),
          });
        }
      }
      lanes = outputs.clone();
      GraphRow {
        item: commit.hash.clone(),
        node: commit.hash.clone(),
        parents: commit.parents.clone(),
        inputs,
        outputs,
      }
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn commit(hash: &str, parents: &[&str]) -> Commit {
    Commit {
      hash: hash.into(),
      parents: parents.iter().map(|parent| (*parent).to_owned()).collect(),
      author: String::new(),
      email: String::new(),
      relative: String::new(),
      date: String::new(),
      refs: String::new(),
      subject: String::new(),
    }
  }

  fn ids(lanes: &[GraphLane]) -> Vec<&str> {
    lanes.iter().map(|lane| lane.id.as_str()).collect()
  }

  #[test]
  fn a_straight_history_is_one_lane() {
    let rows = lay_out(&[commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])]);
    assert_eq!(ids(&rows[0].inputs), Vec::<&str>::new());
    assert_eq!(ids(&rows[0].outputs), ["b"]);
    assert_eq!(ids(&rows[1].inputs), ["b"]);
    assert_eq!(ids(&rows[1].outputs), ["a"]);
    assert_eq!(ids(&rows[2].outputs), Vec::<&str>::new());
    assert!(
      rows
        .iter()
        .all(|row| row.outputs.iter().all(|lane| lane.color == 0))
    );
  }

  #[test]
  fn a_merge_opens_a_lane_that_joins_again() {
    // m merges b (from a side branch) into a; both come from base.
    let rows = lay_out(&[
      commit("m", &["a", "b"]),
      commit("b", &["base"]),
      commit("a", &["base"]),
      commit("base", &[]),
    ]);
    assert_eq!(ids(&rows[0].outputs), ["a", "b"]);
    assert_ne!(rows[0].outputs[0].color, rows[0].outputs[1].color);
    // b sits on the second lane and hands it to base.
    assert_eq!(ids(&rows[1].inputs), ["a", "b"]);
    assert_eq!(ids(&rows[1].outputs), ["a", "base"]);
    assert_eq!(ids(&rows[2].outputs), ["base", "base"]);
    // Both lanes end at base.
    assert_eq!(ids(&rows[3].inputs), ["base", "base"]);
    assert_eq!(ids(&rows[3].outputs), Vec::<&str>::new());
  }
}
