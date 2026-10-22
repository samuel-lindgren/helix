use crate::{graphics::Rect, View, ViewId};
use slotmap::SlotMap;
use std::ops::{Index, IndexMut};

// the dimensions are recomputed on window resize/tree change.
//
#[derive(Debug)]
pub struct Tree {
    root: ViewId,
    // (container, index inside the container)
    pub focus: ViewId,
    // fullscreen: bool,
    area: Rect,

    nodes: SlotMap<ViewId, Node>,

    // used for traversals
    stack: Vec<(ViewId, Rect)>,
}

#[derive(Debug)]
pub struct Node {
    parent: ViewId,
    content: Content,
}

#[derive(Debug)]
pub enum Content {
    View(Box<View>),
    Container(Box<Container>),
}

pub enum Resize {
    Shrink,
    Grow,
}

pub enum Dimension {
    Width,
    Height,
}

impl Node {
    pub fn container(layout: Layout) -> Self {
        Self {
            parent: ViewId::default(),
            content: Content::Container(Box::new(Container::new(layout))),
        }
    }

    pub fn view(view: View) -> Self {
        Self {
            parent: ViewId::default(),
            content: Content::View(Box::new(view)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Horizontal,
    Vertical,
    // could explore stacked/tabbed
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug)]
pub struct Container {
    layout: Layout,
    children: Children,
    area: Rect,
}

#[derive(Debug)]
struct Children {
    pub views: Vec<ViewId>,
    pub bounds: Vec<ContainerBounds>,
}

impl Children {
    fn new() -> Self {
        Children {
            views: Vec::new(),
            bounds: Vec::new(),
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ViewId> {
        self.views.iter()
    }

    pub fn len(&self) -> usize {
        debug_assert_eq!(self.views.len(), self.bounds.len());
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Children {
    fn push(&mut self, node: ViewId) {
        self.views.push(node);

        let portion = 1.0 / self.views.len() as f64;

        let prev = 1.0 - portion;
        for bound in &mut self.bounds {
            bound.portion *= prev;
        }

        self.bounds.push(ContainerBounds { portion });
    }

    fn insert(&mut self, index: usize, node: ViewId) {
        self.views.insert(index, node);

        let portion = 1.0 / self.views.len() as f64;

        let prev = 1.0 - portion;
        for bound in &mut self.bounds {
            bound.portion *= prev;
        }

        self.bounds.insert(index, ContainerBounds { portion });
    }

    fn pop(&mut self) -> Option<ViewId> {
        let bound = self.bounds.pop()?;

        let prev = 1.0 - bound.portion;
        for bound in &mut self.bounds {
            bound.portion /= prev;
        }

        self.views.pop()
    }

    fn remove(&mut self, index: usize) -> ViewId {
        let bound = self.bounds.remove(index);

        let prev = 1.0 - bound.portion;
        for bound in &mut self.bounds {
            bound.portion /= prev;
        }

        self.views.remove(index)
    }

    /// Grows the child at `index` by `amount` of the container, or shrinks it
    /// for a negative amount. Growing takes the space from the siblings on
    /// both sides, nearest first, and never leaves one below `min`, so the
    /// child grows less when they have no more to give. Shrinking stops at
    /// `min` and hands the space to the adjacent siblings.
    fn resize(&mut self, index: usize, amount: f64, min: f64) {
        let bounds = &mut self.bounds;
        let before = index > 0;
        let after = index + 1 < bounds.len();

        if amount < 0.0 {
            let freed = (-amount).min(bounds[index].portion - min).max(0.0);
            match (before, after) {
                (true, true) => {
                    bounds[index - 1].portion += freed / 2.0;
                    bounds[index + 1].portion += freed / 2.0;
                }
                (true, false) => bounds[index - 1].portion += freed,
                (false, true) => bounds[index + 1].portion += freed,
                (false, false) => return,
            }
            bounds[index].portion -= freed;
            return;
        }

        let spare = |bound: &ContainerBounds| (bound.portion - min).max(0.0);
        let spare_before: f64 = bounds[..index].iter().map(spare).sum();
        let spare_after: f64 = bounds[index + 1..].iter().map(spare).sum();

        // Half from each side, or all from the only one; a side with space
        // left covers what the other cannot give.
        let (want_before, want_after) = match (before, after) {
            (true, true) => (amount / 2.0, amount / 2.0),
            (true, false) => (amount, 0.0),
            (false, _) => (0.0, amount),
        };
        let mut take_before = want_before.min(spare_before);
        let mut take_after = want_after.min(spare_after);
        let missing = amount - take_before - take_after;
        let extra = missing.min(spare_before - take_before).max(0.0);
        take_before += extra;
        take_after += (missing - extra).min(spare_after - take_after).max(0.0);

        bounds[index].portion += take_before + take_after;
        for bound in bounds[..index].iter_mut().rev() {
            let take = take_before.min(spare(bound));
            bound.portion -= take;
            take_before -= take;
        }
        for bound in &mut bounds[index + 1..] {
            let take = take_after.min(spare(bound));
            bound.portion -= take;
            take_after -= take;
        }
    }
}

impl<Idx> Index<Idx> for Children
where
    Idx: std::slice::SliceIndex<[ViewId]>,
{
    type Output = Idx::Output;
    fn index(&self, index: Idx) -> &Self::Output {
        self.views.index(index)
    }
}

impl<Idx> IndexMut<Idx> for Children
where
    Idx: std::slice::SliceIndex<[ViewId]>,
{
    #[inline(always)]
    fn index_mut(&mut self, index: Idx) -> &mut Self::Output {
        self.views.index_mut(index)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ContainerBounds {
    portion: f64,
}

impl ContainerBounds {
    /// The whole cells of `size` that the portion covers. The tolerance keeps a
    /// rounding error in the portion (1.9999… for 2) from costing a cell.
    fn cells(self, size: u16) -> u16 {
        (size as f64 * self.portion + 1e-6).floor() as u16
    }
}

impl Container {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            children: Children::new(),
            area: Rect::default(),
        }
    }

    /// The cells the children share: the height of a horizontal container,
    /// the width less the gaps between the views of a vertical one.
    fn shared_size(&self) -> u16 {
        match self.layout {
            Layout::Horizontal => self.area.height,
            Layout::Vertical => {
                let gaps = (self.children.len() as u16).saturating_sub(2);
                self.area.width.saturating_sub(gaps)
            }
        }
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new(Layout::Vertical)
    }
}

impl Tree {
    pub fn new(area: Rect) -> Self {
        let root = Node::container(Layout::Vertical);

        let mut nodes = SlotMap::with_key();
        let root = nodes.insert(root);

        // root is it's own parent
        nodes[root].parent = root;

        Self {
            root,
            focus: root,
            // fullscreen: false,
            area,
            nodes,
            stack: Vec::new(),
        }
    }

    pub fn insert(&mut self, view: View) -> ViewId {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;
        let mut node = Node::view(view);
        node.parent = parent;
        let node = self.nodes.insert(node);
        self.get_mut(node).id = node;

        let container = match &mut self.nodes[parent] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        };

        // insert node after the current item if there is children already
        let pos = if container.children.is_empty() {
            0
        } else {
            let pos = container
                .children
                .iter()
                .position(|&child| child == focus)
                .unwrap();
            pos + 1
        };

        container.children.insert(pos, node);
        // focus the new node
        self.focus = node;

        // recalculate all the sizes
        self.recalculate();

        node
    }

    pub fn split(&mut self, view: View, layout: Layout) -> ViewId {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;

        let node = Node::view(view);
        let node = self.nodes.insert(node);
        self.get_mut(node).id = node;

        let container = match &mut self.nodes[parent] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        };
        if container.layout == layout {
            // insert node after the current item if there is children already
            let pos = if container.children.is_empty() {
                0
            } else {
                let pos = container
                    .children
                    .iter()
                    .position(|&child| child == focus)
                    .unwrap();
                pos + 1
            };
            container.children.insert(pos, node);
            self.nodes[node].parent = parent;
        } else {
            let mut split = Node::container(layout);
            split.parent = parent;
            let split = self.nodes.insert(split);

            let container = match &mut self.nodes[split] {
                Node {
                    content: Content::Container(container),
                    ..
                } => container,
                _ => unreachable!(),
            };
            container.children.push(focus);
            container.children.push(node);
            self.nodes[focus].parent = split;
            self.nodes[node].parent = split;

            let container = match &mut self.nodes[parent] {
                Node {
                    content: Content::Container(container),
                    ..
                } => container,
                _ => unreachable!(),
            };

            let pos = container
                .children
                .iter()
                .position(|&child| child == focus)
                .unwrap();

            // replace focus on parent with split
            container.children[pos] = split;
        }

        // focus the new node
        self.focus = node;

        // recalculate all the sizes
        self.recalculate();

        node
    }

    /// Get a mutable reference to a [Container] by index.
    /// # Panics
    /// Panics if `index` is not in self.nodes, or if the node's content is not a [Content::Container].
    fn container_mut(&mut self, index: ViewId) -> &mut Container {
        match &mut self.nodes[index] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        }
    }

    fn remove_or_replace(&mut self, child: ViewId, replacement: Option<ViewId>) {
        let parent = self.nodes[child].parent;

        self.nodes.remove(child);

        let container = self.container_mut(parent);
        let pos = container
            .children
            .iter()
            .position(|&item| item == child)
            .unwrap();

        if let Some(new) = replacement {
            container.children[pos] = new;
            self.nodes[new].parent = parent;
        } else {
            container.children.remove(pos);
        }
    }

    pub fn remove(&mut self, index: ViewId) {
        if self.focus == index {
            // focus on something else
            self.focus = self.prev();
        }

        let parent = self.nodes[index].parent;
        let parent_is_root = parent == self.root;

        self.remove_or_replace(index, None);

        let parent_container = self.container_mut(parent);
        if parent_container.children.len() == 1 && !parent_is_root {
            // Lets merge the only child back to its grandparent so that Views
            // are equally spaced.
            let sibling = parent_container.children.pop().unwrap();
            self.remove_or_replace(parent, Some(sibling));
        }

        self.recalculate()
    }

    pub fn views(&self) -> impl Iterator<Item = (&View, bool)> {
        let focus = self.focus;
        self.nodes.iter().filter_map(move |(key, node)| match node {
            Node {
                content: Content::View(view),
                ..
            } => Some((view.as_ref(), focus == key)),
            _ => None,
        })
    }

    pub fn views_mut(&mut self) -> impl Iterator<Item = (&mut View, bool)> {
        let focus = self.focus;
        self.nodes
            .iter_mut()
            .filter_map(move |(key, node)| match node {
                Node {
                    content: Content::View(view),
                    ..
                } => Some((view.as_mut(), focus == key)),
                _ => None,
            })
    }

    /// Get reference to a [View] by index.
    /// # Panics
    ///
    /// Panics if `index` is not in self.nodes, or if the node's content is not [Content::View]. This can be checked with [Self::contains].
    pub fn get(&self, index: ViewId) -> &View {
        self.try_get(index).unwrap()
    }

    /// Try to get reference to a [View] by index. Returns `None` if node content is not a [`Content::View`].
    ///
    /// Does not panic if the view does not exists anymore.
    pub fn try_get(&self, index: ViewId) -> Option<&View> {
        match self.nodes.get(index) {
            Some(Node {
                content: Content::View(view),
                ..
            }) => Some(view),
            _ => None,
        }
    }

    /// Get a mutable reference to a [View] by index.
    /// # Panics
    ///
    /// Panics if `index` is not in self.nodes, or if the node's content is not [Content::View]. This can be checked with [Self::contains].
    pub fn get_mut(&mut self, index: ViewId) -> &mut View {
        match &mut self.nodes[index] {
            Node {
                content: Content::View(view),
                ..
            } => view,
            _ => unreachable!(),
        }
    }

    /// Check if tree contains a [Node] with a given index.
    pub fn contains(&self, index: ViewId) -> bool {
        self.nodes.contains_key(index)
    }

    pub fn is_empty(&self) -> bool {
        match &self.nodes[self.root] {
            Node {
                content: Content::Container(container),
                ..
            } => container.children.is_empty(),
            _ => unreachable!(),
        }
    }

    pub fn resize(&mut self, area: Rect) -> bool {
        if self.area != area {
            self.area = area;
            self.recalculate();
            return true;
        }
        false
    }

    pub fn recalculate(&mut self) {
        if self.is_empty() {
            // There are no more views, so the tree should focus itself again.
            self.focus = self.root;

            return;
        }

        self.stack.push((self.root, self.area));

        // take the area
        // fetch the node
        // a) node is view, give it whole area
        // b) node is container, calculate areas for each child and push them on the stack

        while let Some((key, area)) = self.stack.pop() {
            let node = &mut self.nodes[key];

            match &mut node.content {
                Content::View(view) => {
                    // debug!!("setting view area {:?}", area);
                    view.area = area;
                } // TODO: call f()
                Content::Container(container) => {
                    // debug!!("setting container area {:?}", area);
                    container.area = area;

                    match container.layout {
                        Layout::Horizontal => {
                            let len = container.children.len();

                            let mut child_y = area.y;

                            for (i, child) in container.children.iter().enumerate() {
                                let bounds = container.children.bounds[i];
                                let height = bounds.cells(area.height);
                                // never past the container, whatever the portions add up to
                                let height = height.min(area.bottom() - child_y);

                                let mut area = Rect::new(
                                    container.area.x,
                                    child_y,
                                    container.area.width,
                                    height,
                                );
                                child_y += height;

                                // last child takes the remaining height because we can get uneven
                                // space from rounding
                                if i == len - 1 {
                                    area.height = container.area.y + container.area.height - area.y;
                                }

                                self.stack.push((*child, area));
                            }
                        }
                        Layout::Vertical => {
                            let len = container.children.len();

                            let inner_gap = 1u16;
                            let used_area = container.shared_size();

                            let mut child_x = area.x;

                            for (i, child) in container.children.iter().enumerate() {
                                let bounds = container.children.bounds[i];
                                let width = bounds.cells(used_area);
                                // never past the container, whatever the portions add up to
                                let width = width.min(area.right() - child_x);

                                let mut area = Rect::new(
                                    child_x,
                                    container.area.y,
                                    width,
                                    container.area.height,
                                );
                                child_x = (child_x + width + inner_gap).min(container.area.right());

                                // last child takes the remaining width because we can get uneven
                                // space from rounding
                                if i == len - 1 {
                                    area.width = container.area.right() - area.x;
                                }

                                self.stack.push((*child, area));
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn traverse(&self) -> Traverse<'_> {
        Traverse::new(self)
    }

    // Finds the split in the given direction if it exists
    pub fn find_split_in_direction(&self, id: ViewId, direction: Direction) -> Option<ViewId> {
        let parent = self.nodes[id].parent;
        // Base case, we found the root of the tree
        if parent == id {
            return None;
        }
        // Parent must always be a container
        let parent_container = match &self.nodes[parent].content {
            Content::Container(container) => container,
            Content::View(_) => unreachable!(),
        };

        match (direction, parent_container.layout) {
            (Direction::Up, Layout::Vertical)
            | (Direction::Left, Layout::Horizontal)
            | (Direction::Right, Layout::Horizontal)
            | (Direction::Down, Layout::Vertical) => {
                // The desired direction of movement is not possible within
                // the parent container so the search must continue closer to
                // the root of the split tree.
                self.find_split_in_direction(parent, direction)
            }
            (Direction::Up, Layout::Horizontal)
            | (Direction::Down, Layout::Horizontal)
            | (Direction::Left, Layout::Vertical)
            | (Direction::Right, Layout::Vertical) => {
                // It's possible to move in the desired direction within
                // the parent container so an attempt is made to find the
                // correct child.
                match self.find_child(id, &parent_container.children.views, direction) {
                    // Child is found, search is ended
                    Some(id) => Some(id),
                    // A child is not found. This could be because of either two scenarios
                    // 1. Its not possible to move in the desired direction, and search should end
                    // 2. A layout like the following with focus at X and desired direction Right
                    // | _ | x |   |
                    // | _ _ _ |   |
                    // | _ _ _ |   |
                    // The container containing X ends at X so no rightward movement is possible
                    // however there still exists another view/container to the right that hasn't
                    // been explored. Thus another search is done here in the parent container
                    // before concluding it's not possible to move in the desired direction.
                    None => self.find_split_in_direction(parent, direction),
                }
            }
        }
    }

    fn find_child(&self, id: ViewId, children: &[ViewId], direction: Direction) -> Option<ViewId> {
        let mut child_id = match direction {
            // index wise in the child list the Up and Left represents a -1
            // thus reversed iterator.
            Direction::Up | Direction::Left => children
                .iter()
                .rev()
                .skip_while(|i| **i != id)
                .copied()
                .nth(1)?,
            // Down and Right => +1 index wise in the child list
            Direction::Down | Direction::Right => {
                children.iter().skip_while(|i| **i != id).copied().nth(1)?
            }
        };
        let (current_x, current_y) = match &self.nodes[self.focus].content {
            Content::View(current_view) => (current_view.area.left(), current_view.area.top()),
            Content::Container(_) => unreachable!(),
        };

        // If the child is a container the search finds the closest container child
        // visually based on screen location.
        while let Content::Container(container) = &self.nodes[child_id].content {
            match (direction, container.layout) {
                (_, Layout::Vertical) => {
                    // find closest split based on x because y is irrelevant
                    // in a vertical container (and already correct based on previous search)
                    child_id = *container.children.iter().min_by_key(|id| {
                        let x = match &self.nodes[**id].content {
                            Content::View(view) => view.area.left(),
                            Content::Container(container) => container.area.left(),
                        };
                        (current_x as i16 - x as i16).abs()
                    })?;
                }
                (_, Layout::Horizontal) => {
                    // find closest split based on y because x is irrelevant
                    // in a horizontal container (and already correct based on previous search)
                    child_id = *container.children.iter().min_by_key(|id| {
                        let y = match &self.nodes[**id].content {
                            Content::View(view) => view.area.top(),
                            Content::Container(container) => container.area.top(),
                        };
                        (current_y as i16 - y as i16).abs()
                    })?;
                }
            }
        }
        Some(child_id)
    }

    pub fn prev(&self) -> ViewId {
        // This function is very dumb, but that's because we don't store any parent links.
        // (we'd be able to go parent.prev_sibling() recursively until we find something)
        // For now that's okay though, since it's unlikely you'll be able to open a large enough
        // number of splits to notice.

        let mut views = self
            .traverse()
            .rev()
            .skip_while(|&(id, _view)| id != self.focus)
            .skip(1); // Skip focused value
        if let Some((id, _)) = views.next() {
            id
        } else {
            // extremely crude, take the last item
            let (key, _) = self.traverse().next_back().unwrap();
            key
        }
    }

    pub fn next(&self) -> ViewId {
        // This function is very dumb, but that's because we don't store any parent links.
        // (we'd be able to go parent.next_sibling() recursively until we find something)
        // For now that's okay though, since it's unlikely you'll be able to open a large enough
        // number of splits to notice.

        let mut views = self
            .traverse()
            .skip_while(|&(id, _view)| id != self.focus)
            .skip(1); // Skip focused value
        if let Some((id, _)) = views.next() {
            id
        } else {
            // extremely crude, take the first item again
            let (key, _) = self.traverse().next().unwrap();
            key
        }
    }

    pub fn transpose(&mut self) {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;
        if let Content::Container(container) = &mut self.nodes[parent].content {
            container.layout = match container.layout {
                Layout::Vertical => Layout::Horizontal,
                Layout::Horizontal => Layout::Vertical,
            };
            self.recalculate();
        }
    }

    fn find_container(&mut self, layout: Layout) -> Option<(&mut Container, usize)> {
        let mut focus = self.focus;
        let mut parent = self.nodes[focus].parent;

        let mut found = false;
        while parent != focus {
            let Content::Container(container) = &self.nodes[parent].content else {
                unreachable!("parent should be a container");
            };

            if container.layout == layout {
                found = true;
                break;
            }

            focus = parent;
            parent = self.nodes[focus].parent;
        }

        if found {
            let Content::Container(container) = &mut self.nodes[parent].content else {
                unreachable!("parent should be a container");
            };

            let idx = container.children.iter().position(|node| *node == focus)?;
            Some((container, idx))
        } else {
            None
        }
    }

    pub fn resize_view(&mut self, resize_type: Resize, dimension: Dimension, count: usize) {
        let layout = match dimension {
            Dimension::Width => Layout::Vertical,
            Dimension::Height => Layout::Horizontal,
        };

        if let Some((container, idx)) = self.find_container(layout) {
            let size = container.shared_size();
            if size == 0 {
                return;
            }
            let cell = 1.0 / size as f64;

            // two cells per step, so that a view between two others moves both
            // of its edges by a whole cell
            let diff = 2.0 * cell * count as f64;
            let diff = match resize_type {
                Resize::Shrink => -diff,
                Resize::Grow => diff,
            };

            // 5% of the container, and room for a line and the statusline
            let min = f64::max(0.05, 2.0 * cell);

            container.children.resize(idx, diff, min);
            self.recalculate();
        }
    }

    pub fn swap_split_in_direction(&mut self, direction: Direction) -> Option<()> {
        let focus = self.focus;
        let target = self.find_split_in_direction(focus, direction)?;
        let focus_parent = self.nodes[focus].parent;
        let target_parent = self.nodes[target].parent;

        if focus_parent == target_parent {
            let parent = focus_parent;
            let [parent, focus, target] = self.nodes.get_disjoint_mut([parent, focus, target])?;
            match (&mut parent.content, &mut focus.content, &mut target.content) {
                (
                    Content::Container(parent),
                    Content::View(focus_view),
                    Content::View(target_view),
                ) => {
                    let focus_pos = parent.children.iter().position(|id| focus_view.id == *id)?;
                    let target_pos = parent
                        .children
                        .iter()
                        .position(|id| target_view.id == *id)?;
                    // swap node positions so that traversal order is kept
                    parent.children[focus_pos] = target_view.id;
                    parent.children[target_pos] = focus_view.id;
                    // swap area so that views rendered at the correct location
                    std::mem::swap(&mut focus_view.area, &mut target_view.area);

                    Some(())
                }
                _ => unreachable!(),
            }
        } else {
            let [focus_parent, target_parent, focus, target] =
                self.nodes
                    .get_disjoint_mut([focus_parent, target_parent, focus, target])?;
            match (
                &mut focus_parent.content,
                &mut target_parent.content,
                &mut focus.content,
                &mut target.content,
            ) {
                (
                    Content::Container(focus_parent),
                    Content::Container(target_parent),
                    Content::View(focus_view),
                    Content::View(target_view),
                ) => {
                    let focus_pos = focus_parent
                        .children
                        .iter()
                        .position(|id| focus_view.id == *id)?;
                    let target_pos = target_parent
                        .children
                        .iter()
                        .position(|id| target_view.id == *id)?;
                    // re-parent target and focus nodes
                    std::mem::swap(
                        &mut focus_parent.children[focus_pos],
                        &mut target_parent.children[target_pos],
                    );
                    std::mem::swap(&mut focus.parent, &mut target.parent);
                    // swap area so that views rendered at the correct location
                    std::mem::swap(&mut focus_view.area, &mut target_view.area);

                    Some(())
                }
                _ => unreachable!(),
            }
        }
    }

    pub fn area(&self) -> Rect {
        self.area
    }
}

#[derive(Debug)]
pub struct Traverse<'a> {
    tree: &'a Tree,
    stack: Vec<ViewId>, // TODO: reuse the one we use on update
}

impl<'a> Traverse<'a> {
    fn new(tree: &'a Tree) -> Self {
        Self {
            tree,
            stack: vec![tree.root],
        }
    }
}

impl<'a> Iterator for Traverse<'a> {
    type Item = (ViewId, &'a View);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let key = self.stack.pop()?;

            let node = &self.tree.nodes[key];

            match &node.content {
                Content::View(view) => return Some((key, view)),
                Content::Container(container) => {
                    self.stack.extend(container.children.iter().rev());
                }
            }
        }
    }
}

impl DoubleEndedIterator for Traverse<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        loop {
            let key = self.stack.pop()?;

            let node = &self.tree.nodes[key];

            match &node.content {
                Content::View(view) => return Some((key, view)),
                Content::Container(container) => {
                    self.stack.extend(container.children.iter());
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::editor::GutterConfig;
    use crate::DocumentId;

    #[test]
    fn find_split_in_direction() {
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: 180,
            height: 80,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let l0 = tree.focus;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let r0 = tree.focus;

        tree.focus = l0;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Horizontal);
        let l1 = tree.focus;

        tree.focus = l0;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        // Tree in test
        // | L0  | L2 |    |
        // |    L1    | R0 |
        let l2 = tree.focus;
        assert_eq!(Some(l0), tree.find_split_in_direction(l2, Direction::Left));
        assert_eq!(Some(l1), tree.find_split_in_direction(l2, Direction::Down));
        assert_eq!(Some(r0), tree.find_split_in_direction(l2, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(l2, Direction::Up));

        tree.focus = l1;
        assert_eq!(None, tree.find_split_in_direction(l1, Direction::Left));
        assert_eq!(None, tree.find_split_in_direction(l1, Direction::Down));
        assert_eq!(Some(r0), tree.find_split_in_direction(l1, Direction::Right));
        assert_eq!(Some(l0), tree.find_split_in_direction(l1, Direction::Up));

        tree.focus = l0;
        assert_eq!(None, tree.find_split_in_direction(l0, Direction::Left));
        assert_eq!(Some(l1), tree.find_split_in_direction(l0, Direction::Down));
        assert_eq!(Some(l2), tree.find_split_in_direction(l0, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(l0, Direction::Up));

        tree.focus = r0;
        assert_eq!(Some(l2), tree.find_split_in_direction(r0, Direction::Left));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Down));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Up));
    }

    #[test]
    fn swap_split_in_direction() {
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: 180,
            height: 80,
        });

        let doc_l0 = DocumentId::default();
        let mut view = View::new(doc_l0, GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let l0 = tree.focus;

        let doc_r0 = DocumentId::default();
        let view = View::new(doc_r0, GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let r0 = tree.focus;

        tree.focus = l0;

        let doc_l1 = DocumentId::default();
        let view = View::new(doc_l1, GutterConfig::default());
        tree.split(view, Layout::Horizontal);
        let l1 = tree.focus;

        tree.focus = l0;

        let doc_l2 = DocumentId::default();
        let view = View::new(doc_l2, GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let l2 = tree.focus;

        // Views in test
        // | L0  | L2 |    |
        // |    L1    | R0 |

        // Document IDs in test
        // | l0  | l2 |    |
        // |    l1    | r0 |

        fn doc_id(tree: &Tree, view_id: ViewId) -> Option<DocumentId> {
            if let Content::View(view) = &tree.nodes[view_id].content {
                Some(view.doc)
            } else {
                None
            }
        }

        tree.focus = l0;
        // `*` marks the view in focus from view table (here L0)
        // | l0*  | l2 |    |
        // |    l1     | r0 |
        tree.swap_split_in_direction(Direction::Down);
        // | l1   | l2 |    |
        // |    l0*    | r0 |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_l0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_r0));

        tree.swap_split_in_direction(Direction::Right);

        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        // cannot swap, nothing changes
        tree.swap_split_in_direction(Direction::Up);
        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        // cannot swap, nothing changes
        tree.swap_split_in_direction(Direction::Down);
        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        tree.focus = l2;
        // | l1  | l2* |    |
        // |    r0     | l0 |

        tree.swap_split_in_direction(Direction::Down);
        // | l1  | r0  |    |
        // |    l2*    | l0 |
        assert_eq!(tree.focus, l2);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_l2));
        assert_eq!(doc_id(&tree, l2), Some(doc_r0));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        tree.swap_split_in_direction(Direction::Up);
        // | l2* | r0 |    |
        // |    l1    | l0 |
        assert_eq!(tree.focus, l2);
        assert_eq!(doc_id(&tree, l0), Some(doc_l2));
        assert_eq!(doc_id(&tree, l1), Some(doc_l1));
        assert_eq!(doc_id(&tree, l2), Some(doc_r0));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));
    }

    #[test]
    fn all_vertical_views_have_same_width() {
        let tree_area_width = 180;
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: tree_area_width,
            height: 80,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Horizontal);

        tree.remove(tree.focus);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        // Make sure that we only have one level in the tree.
        assert_eq!(3, tree.views().count());
        assert_eq!(
            vec![
                tree_area_width / 3 - 1, // gap here
                tree_area_width / 3 - 1, // gap here
                tree_area_width / 3
            ],
            tree.views()
                .map(|(view, _)| view.area.width)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn vsplit_gap_rounding() {
        let (tree_area_width, tree_area_height) = (80, 24);
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: tree_area_width,
            height: tree_area_height,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, tree_area_width, tree_area_height);
        tree.insert(view);

        for _ in 0..9 {
            let view = View::new(DocumentId::default(), GutterConfig::default());
            tree.split(view, Layout::Vertical);
        }

        assert_eq!(10, tree.views().count());
        assert_eq!(
            std::iter::repeat_n(7, 9)
                .chain(Some(8)) // Rounding in `recalculate`.
                .collect::<Vec<_>>(),
            tree.views()
                .map(|(view, _)| view.area.width)
                .collect::<Vec<_>>()
        );
    }

    /// A tree of `count` views in one container with `layout`, in an 80x40 area,
    /// and the ids of the views in order.
    fn split_tree(layout: Layout, count: usize) -> (Tree, Vec<ViewId>) {
        let mut tree = Tree::new(Rect::new(0, 0, 80, 40));
        let mut ids = vec![tree.insert(View::new(DocumentId::default(), GutterConfig::default()))];
        for _ in 1..count {
            let view = View::new(DocumentId::default(), GutterConfig::default());
            ids.push(tree.split(view, layout));
        }
        (tree, ids)
    }

    fn heights(tree: &Tree, ids: &[ViewId]) -> Vec<u16> {
        ids.iter().map(|id| tree.get(*id).area.height).collect()
    }

    fn widths(tree: &Tree, ids: &[ViewId]) -> Vec<u16> {
        ids.iter().map(|id| tree.get(*id).area.width).collect()
    }

    /// The views of a horizontal container are stacked without gaps or
    /// overlap and fill its height.
    fn assert_stacked(tree: &Tree, ids: &[ViewId]) {
        let mut y = tree.area.top();
        for id in ids {
            let area = tree.get(*id).area;
            assert_eq!(y, area.top(), "{:?}", heights(tree, ids));
            y = area.bottom();
        }
        assert_eq!(tree.area.bottom(), y, "{:?}", heights(tree, ids));
    }

    /// The views of a vertical container sit side by side with one column
    /// between them and fill its width.
    fn assert_side_by_side(tree: &Tree, ids: &[ViewId]) {
        let mut x = tree.area.left();
        for id in ids {
            let area = tree.get(*id).area;
            assert_eq!(x, area.left(), "{:?}", widths(tree, ids));
            x = area.right() + 1;
        }
        assert_eq!(tree.area.right() + 1, x, "{:?}", widths(tree, ids));
    }

    #[test]
    fn resize_moves_the_border_two_cells_per_step() {
        let (mut tree, ids) = split_tree(Layout::Horizontal, 2);
        tree.focus = ids[0];

        for step in 1..=5 {
            tree.resize_view(Resize::Grow, Dimension::Height, 1);
            assert_eq!(vec![20 + 2 * step, 20 - 2 * step], heights(&tree, &ids));
        }
        tree.resize_view(Resize::Shrink, Dimension::Height, 3);
        assert_eq!(vec![24, 16], heights(&tree, &ids));
        assert_stacked(&tree, &ids);
    }

    #[test]
    fn resize_keeps_a_minimum_size() {
        let (mut tree, ids) = split_tree(Layout::Horizontal, 2);
        tree.focus = ids[1];

        tree.resize_view(Resize::Shrink, Dimension::Height, 100);
        assert_eq!(vec![38, 2], heights(&tree, &ids));
        tree.resize_view(Resize::Grow, Dimension::Height, 100);
        assert_eq!(vec![2, 38], heights(&tree, &ids));
    }

    #[test]
    fn resize_middle_of_three_stays_inside_the_container() {
        let (mut tree, ids) = split_tree(Layout::Horizontal, 3);
        tree.focus = ids[2];
        tree.resize_view(Resize::Shrink, Dimension::Height, 100);
        tree.focus = ids[1];
        tree.resize_view(Resize::Grow, Dimension::Height, 100);
        assert_eq!(vec![2, 36, 2], heights(&tree, &ids));
        assert_stacked(&tree, &ids);

        let (mut tree, ids) = split_tree(Layout::Vertical, 3);
        tree.focus = ids[2];
        tree.resize_view(Resize::Shrink, Dimension::Width, 100);
        tree.focus = ids[1];
        tree.resize_view(Resize::Grow, Dimension::Width, 100);
        assert_side_by_side(&tree, &ids);
        assert!(widths(&tree, &ids).iter().all(|width| *width >= 3));
    }

    #[test]
    fn resize_takes_from_the_next_sibling_when_the_nearest_is_at_its_minimum() {
        let (mut tree, ids) = split_tree(Layout::Horizontal, 3);
        tree.focus = ids[0];

        tree.resize_view(Resize::Grow, Dimension::Height, 100);
        assert_eq!(vec![36, 2, 2], heights(&tree, &ids));
        assert_stacked(&tree, &ids);

        tree.focus = ids[2];
        tree.resize_view(Resize::Grow, Dimension::Height, 100);
        assert_eq!(vec![2, 2, 36], heights(&tree, &ids));
        assert_stacked(&tree, &ids);
    }

    #[test]
    fn resize_with_more_views_than_fit_at_their_minimum() {
        // Twenty-five views cannot all keep 5% of the container.
        let (mut tree, mut ids) = split_tree(Layout::Horizontal, 2);
        tree.focus = ids[0];
        tree.resize_view(Resize::Grow, Dimension::Height, 100);
        for _ in 0..23 {
            let view = View::new(DocumentId::default(), GutterConfig::default());
            ids.insert(1, tree.split(view, Layout::Horizontal));
            tree.focus = ids[0];
        }

        tree.resize_view(Resize::Grow, Dimension::Height, 3);
        assert_stacked(&tree, &ids);
        tree.resize_view(Resize::Shrink, Dimension::Height, 3);
        assert_stacked(&tree, &ids);
    }

    #[test]
    fn views_tile_the_area_through_splits_resizes_and_closes() {
        // A fixed pseudo-random sequence of operations; after each, the views
        // cover the whole area without overlapping.
        let area = Rect::new(0, 0, 97, 31);
        let mut tree = Tree::new(area);
        tree.insert(View::new(DocumentId::default(), GutterConfig::default()));
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };

        for step in 0..5000 {
            let views: Vec<ViewId> = tree.views().map(|(view, _)| view.id).collect();
            tree.focus = views[next(views.len() as u64) as usize];
            let count = 1 + next(30) as usize;
            match next(10) {
                0 | 1 if views.len() < 12 => {
                    let view = View::new(DocumentId::default(), GutterConfig::default());
                    tree.split(view, Layout::Horizontal);
                }
                2 | 3 if views.len() < 12 => {
                    let view = View::new(DocumentId::default(), GutterConfig::default());
                    tree.split(view, Layout::Vertical);
                }
                4 if views.len() > 1 => tree.remove(tree.focus),
                5 | 6 => tree.resize_view(Resize::Grow, Dimension::Height, count),
                7 => tree.resize_view(Resize::Shrink, Dimension::Height, count),
                8 => tree.resize_view(Resize::Grow, Dimension::Width, count),
                _ => tree.resize_view(Resize::Shrink, Dimension::Width, count),
            }

            let areas: Vec<Rect> = tree.views().map(|(view, _)| view.area).collect();
            for (i, a) in areas.iter().enumerate() {
                assert_eq!(*a, a.intersection(area), "step {step}: {areas:?}");
                for b in &areas[i + 1..] {
                    assert!(a.intersection(*b).area() == 0, "step {step}: {areas:?}");
                }
            }
            // The one-column gaps between side-by-side views are not covered.
            let covered: usize = areas.iter().map(|a| a.area()).sum();
            assert!(covered <= area.area(), "step {step}: {areas:?}");
            assert!(
                covered + 11 * area.height as usize >= area.area(),
                "step {step}: {areas:?}"
            );
        }
    }

    #[test]
    fn closing_a_split_shares_its_space_by_size() {
        let (mut tree, ids) = split_tree(Layout::Horizontal, 3);
        tree.focus = ids[0];
        tree.resize_view(Resize::Grow, Dimension::Height, 4);
        assert_eq!(vec![21, 5, 14], heights(&tree, &ids));

        tree.remove(ids[2]);
        assert_eq!(vec![32, 8], heights(&tree, &ids[..2]));
        assert_stacked(&tree, &ids[..2]);
    }
}
