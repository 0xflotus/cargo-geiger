use crate::format::print::{Prefix, PrintConfig};
use crate::graph::{Graph, Node};
use crate::tree::{get_tree_symbols, TextTreeLine};

use super::construct_tree_vines_string;

use cargo::core::dependency::DepKind;
use cargo::core::PackageId;
use petgraph::visit::EdgeRef;
use petgraph::EdgeDirection;
use std::collections::{HashMap, HashSet};

/// Printing the returned TextTreeLines in order is expected to produce a nice
/// looking tree structure.
///
/// TODO: Return a impl Iterator<Item = TextTreeLine ... >
/// TODO: Consider separating the tree vine building from the tree traversal.
///
pub fn walk_dependency_tree(
    root_pack_id: PackageId,
    graph: &Graph,
    print_config: &PrintConfig,
) -> Vec<TextTreeLine> {
    let mut visited_deps = HashSet::new();
    let mut levels_continue = vec![];
    let node = &graph.graph[graph.nodes[&root_pack_id]];
    walk_dependency_node(
        node,
        graph,
        &mut visited_deps,
        &mut levels_continue,
        print_config,
    )
}

fn walk_dependency_kind(
    kind: DepKind,
    deps: &mut Vec<&Node>,
    graph: &Graph,
    visited_deps: &mut HashSet<PackageId>,
    levels_continue: &mut Vec<bool>,
    print_config: &PrintConfig,
) -> Vec<TextTreeLine> {
    if deps.is_empty() {
        return Vec::new();
    }

    // Resolve uses Hash data types internally but we want consistent output ordering
    deps.sort_by_key(|n| n.id);

    let tree_symbols = get_tree_symbols(print_config.charset);
    let mut output = Vec::new();
    if let Prefix::Indent = print_config.prefix {
        match kind {
            DepKind::Normal => (),
            _ => {
                let mut tree_vines = String::new();
                for &continues in &**levels_continue {
                    let c = if continues { tree_symbols.down } else { " " };
                    tree_vines.push_str(&format!("{}   ", c));
                }
                output.push(TextTreeLine::ExtraDepsGroup { kind, tree_vines });
            }
        }
    }

    let mut it = deps.iter().peekable();
    while let Some(dependency) = it.next() {
        levels_continue.push(it.peek().is_some());
        output.append(&mut walk_dependency_node(
            dependency,
            graph,
            visited_deps,
            levels_continue,
            print_config,
        ));
        levels_continue.pop();
    }
    output
}

fn walk_dependency_node(
    package: &Node,
    graph: &Graph,
    visited_deps: &mut HashSet<PackageId>,
    levels_continue: &mut Vec<bool>,
    print_config: &PrintConfig,
) -> Vec<TextTreeLine> {
    let new = print_config.all || visited_deps.insert(package.id);
    let tree_vines = construct_tree_vines_string(levels_continue, print_config);

    let mut all_out = vec![TextTreeLine::Package {
        id: package.id,
        tree_vines,
    }];

    if !new {
        return all_out;
    }

    let mut dependency_type_nodes: HashMap<DepKind, Vec<&Node>> = [
        (DepKind::Build, vec![]),
        (DepKind::Development, vec![]),
        (DepKind::Normal, vec![]),
    ]
    .iter()
    .cloned()
    .collect();

    for edge in graph
        .graph
        .edges_directed(graph.nodes[&package.id], print_config.direction)
    {
        let dep = match print_config.direction {
            EdgeDirection::Incoming => &graph.graph[edge.source()],
            EdgeDirection::Outgoing => &graph.graph[edge.target()],
        };

        dependency_type_nodes
            .get_mut(edge.weight())
            .unwrap()
            .push(dep);
    }

    for (dep_kind, nodes) in dependency_type_nodes.iter_mut() {
        let mut dep_kind_out = walk_dependency_kind(
            *dep_kind,
            nodes,
            graph,
            visited_deps,
            levels_continue,
            print_config,
        );

        all_out.append(&mut dep_kind_out);
    }

    all_out
}

#[cfg(test)]
mod traversal_tests {
    use super::*;

    use crate::format::pattern::Pattern;
    use crate::format::Charset;

    use cargo::core::shell::Verbosity;
    use geiger::IncludeTests;
    use petgraph::EdgeDirection;

    #[test]
    fn construct_tree_vines_test() {
        let mut levels_continue = vec![true, false, true];
        let pattern = Pattern::try_build("{p}").unwrap();

        let print_config = construct_print_config(&pattern, Prefix::Depth);
        let tree_vines_string =
            construct_tree_vines_string(&mut levels_continue, &print_config);

        assert_eq!(tree_vines_string, "3 ");

        let print_config = construct_print_config(&pattern, Prefix::Indent);
        let tree_vines_string =
            construct_tree_vines_string(&mut levels_continue, &print_config);

        assert_eq!(tree_vines_string, "|       |-- ");

        let print_config = construct_print_config(&pattern, Prefix::None);
        let tree_vines_string =
            construct_tree_vines_string(&mut levels_continue, &print_config);

        assert_eq!(tree_vines_string, "");
    }

    fn construct_print_config(
        pattern: &Pattern,
        prefix: Prefix,
    ) -> PrintConfig {
        PrintConfig {
            all: false,
            verbosity: Verbosity::Verbose,
            direction: EdgeDirection::Outgoing,
            prefix,
            format: pattern,
            charset: Charset::Ascii,
            allow_partial_results: false,
            include_tests: IncludeTests::Yes,
            output_format: None,
        }
    }
}
