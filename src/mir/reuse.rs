use mir::{MirNode, MirNodeRef, MirNodeType, MirQuery};
use slog;

pub fn merge_mir_for_queries(log: &slog::Logger,
                             new_query: &MirQuery,
                             old_query: &MirQuery)
                             -> MirQuery {
    use std::collections::{HashSet, HashMap, VecDeque};
    use std::rc::Rc;
    use std::cell::RefCell;

    let mut trace_nodes = VecDeque::new();
    for old_base in &old_query.roots {
        for new_base in &new_query.roots {
            if old_base.borrow().can_reuse_as(&*new_base.borrow()) {
                trace!(log, "tracing from reusable base {:?}", old_base);
                trace_nodes.push_back((old_base.clone(), new_base.clone()));
            }
        }
    }

    // trace forwards from all matching bases in `old_query`, until no reuseable children are
    // found.
    let mut visited = HashSet::new();
    let mut reuse = HashMap::new();
    while let Some((old, new)) = trace_nodes.pop_front() {
        let new_id = new.borrow().versioned_name();
        // reuseable node found, keep going
        trace!(log,
               "found reuseable node {:?} for {:?}, continuing",
               old,
               new);
        assert!(!reuse.contains_key(&new_id));

        let reuse_node;
        {
            let o_ref = old.clone();
            let o = old.borrow();
            // Note that we manually build the `MirNode` here, rather than calling `MirNode::new()`
            // because `new()` automatically registers the node as a child with its ancestors. We
            // don't want to do this here because we later re-write the ancestors' child that this
            // node replaces to point to this node.
            reuse_node = Rc::new(RefCell::new(MirNode {
                                                  name: o.name.clone(),
                                                  from_version: o.from_version,
                                                  columns: o.columns.clone(),
                                                  inner: MirNodeType::Reuse { node: o_ref },
                                                  ancestors: o.ancestors.clone(),
                                                  children: o.children.clone(),
                                                  flow_node: None,
                                              }));
        }
        reuse.insert(new_id.clone(), reuse_node);

        // look for matching old node children for each of the new node's children.
        // If any are found, we can continue exploring that path, as the new query contains one
        // ore more child nodes from the old query.
        for new_child in new.borrow().children() {
            let new_child_id = new_child.borrow().versioned_name();
            if visited.contains(&new_child_id) {
                trace!(log,
                       "hit previously visited node {:?}, ignoring",
                       new_child_id);
                continue;
            }

            trace!(log, "visiting node {:?}", new_child_id);
            visited.insert(new_child_id.clone());

            let mut found = false;
            for old_child in old.borrow().children() {
                if old_child.borrow().can_reuse_as(&*new_child.borrow()) {
                    trace!(log,
                           "add child {:?} to queue as it has a match",
                           new_child_id);
                    trace_nodes.push_back((old_child.clone(), new_child.clone()));
                    found = true;
                }
            }
            if !found {
                // if no child of this node is reusable, we give up on this path
                trace!(log,
                       "no reuseable node found for {:?} in old query, giving up",
                       new_child);
            }
        }
    }

    // wire in the new `Reuse` nodes
    let mut rewritten_roots = Vec::new();
    let mut rewritten_leaf = new_query.leaf.clone();

    let mut q: VecDeque<MirNodeRef> = new_query.roots.iter().cloned().collect();
    // XXX(malte): should be topological order traversal; currently ends up repeatedly
    // visiting a bunch of nodes unnecessarily
    while let Some(n) = q.pop_front() {
        let ancestors: Vec<_> = n.borrow()
            .ancestors()
            .iter()
            .map(|a| match reuse.get(&a.borrow().versioned_name()) {
                     None => a,
                     Some(ref reused) => reused,
                 })
            .cloned()
            .collect();
        let children: Vec<_> = n.borrow()
            .children()
            .iter()
            .map(|c| match reuse.get(&c.borrow().versioned_name()) {
                     None => c,
                     Some(ref reused) => reused,
                 })
            .cloned()
            .collect();

        let real_n = match reuse.get(&n.borrow().versioned_name()) {
            None => n.clone(),
            Some(reused) => reused.clone(),
        };

        if ancestors.is_empty() {
            rewritten_roots.push(real_n.clone());
        }
        if children.is_empty() {
            rewritten_leaf = real_n.clone();
        }

        real_n.borrow_mut().ancestors = ancestors;
        real_n.borrow_mut().children = children;

        for c in &n.borrow().children {
            q.push_back(c.clone());
        }
    }

    let rewritten_query = MirQuery {
        name: new_query.name.clone(),
        roots: rewritten_roots,
        leaf: rewritten_leaf,
    };

    rewritten_query
}

#[cfg(test)]
mod tests {
    use super::*;
    use nom_sql::Column;
    use mir::{MirNode, MirNodeRef, MirNodeType};

    fn make_nodes() -> (MirNodeRef, MirNodeRef, MirNodeRef, MirNodeRef) {
        let a = MirNode::new("a",
                             0,
                             vec![Column::from("aa"), Column::from("ab")],
                             MirNodeType::Base { keys: vec![Column::from("aa")] },
                             vec![],
                             vec![]);
        let b = MirNode::new("b",
                             0,
                             vec![Column::from("ba"), Column::from("bb")],
                             MirNodeType::Base { keys: vec![Column::from("ba")] },
                             vec![],
                             vec![]);
        let c = MirNode::new("c",
                             0,
                             vec![Column::from("aa"), Column::from("ba")],
                             MirNodeType::Join {
                                 on_left: vec![Column::from("ab")],
                                 on_right: vec![Column::from("bb")],
                                 project: vec![Column::from("aa"), Column::from("ba")],
                             },
                             vec![],
                             vec![]);
        let d = MirNode::new("d",
                             0,
                             vec![Column::from("aa"), Column::from("ba")],
                             MirNodeType::Leaf {
                                 node: c.clone(),
                                 keys: vec![Column::from("ba")],
                             },
                             vec![],
                             vec![]);
        (a, b, c, d)
    }

    #[test]
    fn merge_mir() {
        use mir::{MirNode, MirNodeType, MirQuery};
        use slog::{self, DrainExt};
        use slog_term;

        let log = slog::Logger::root(slog_term::streamer().full().build().fuse(), None);

        let (a, b, c, d) = make_nodes();

        let reuse_a = MirNode::reuse(a, 0);
        reuse_a.borrow_mut().add_child(c.clone());
        b.borrow_mut().add_child(c.clone());
        c.borrow_mut().add_ancestor(reuse_a.clone());
        c.borrow_mut().add_ancestor(b.clone());
        c.borrow_mut().add_child(d.clone());
        d.borrow_mut().add_ancestor(c);

        let mq1 = MirQuery {
            name: String::from("q1"),
            roots: vec![reuse_a, b],
            leaf: d,
        };

        // when merging with ourselves, the result should consist entirely of reuse nodes
        let merged_reflexive = merge_mir_for_queries(&log, &mq1, &mq1);
        assert!(merged_reflexive
                    .topo_nodes()
                    .iter()
                    .all(|n| match n.borrow().inner {
                             MirNodeType::Reuse { .. } => true,
                             _ => false,
                         }));

        /*let mq2 = MirQuery {
            name: String::from("q1"),
            roots: vec![a, b],
            leaf: d,
        };*/

        // let's merge with a test query that permits direct extension
    }
}