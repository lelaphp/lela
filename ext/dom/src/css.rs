//! `parentnode/css_selectors.c` and the selector engine php adapted from
//! lexbor to libxml2 nodes, `lexbor/selectors-adapted/selectors.c`:
//! `querySelector`, `querySelectorAll`, `matches` and `closest` over the
//! list `php_ext_lexbor::ffi::css` parses.
//!
//! The engine is lexbor's state machine as php carries it, one state per
//! function of the C, over entries kept in a vector and named by index
//! where the C links `lxb_selectors_entry_t` and `lxb_selectors_nested_t`
//! by pointer. It is kept whole rather than rewritten as a recursive
//! matcher because its backtracking is observable: `:is(.a) span` tries
//! only the nearest ancestor, and `:nth-child(An+B of S)` keeps the
//! complex selector of `S` it last matched when it moves to the next
//! sibling, and php answers with exactly those nodes.

use php_ext::raising::{Thrown, argument_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, Value};
use php_ext_lexbor::ffi::css::{
    Anb, AttributeMatch, Combinator, Kind as SelectorKind, List, Match, Modifier, PseudoClass,
    PseudoClassFunction, Selector,
};
use php_ext_libxml::ffi::{Kind, Node, Quirks};

use crate::object::Code;
use crate::private::{HTML_NS, html_in_html_document, ns_is};
use crate::throwables::Throwables;
use crate::{dom, fetch, object_for};

/// `lxb_status_t` as the engine answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Stop,
    /// `LXB_STATUS_ERROR`: the column combinator, which php reports as an
    /// unsupported selector.
    Error,
}

/// `lxb_selectors_opt_t` without `MATCH_ROOT`, which php compiles out.
#[derive(Debug, Clone, Copy)]
struct Options {
    match_first: bool,
    quirks: bool,
}

/// What the outermost callback does with a node the list matched: the
/// three callbacks of `css_selectors.c`.
#[derive(Debug)]
enum Sink {
    /// `dom_query_selector_find_single_callback`.
    First(Option<Node>),
    /// `dom_query_selector_find_array_callback`.
    All(Vec<Node>),
    /// `dom_query_selector_find_matches_callback`.
    Matches { reference: Node, found: bool },
}

impl Sink {
    fn accept(&mut self, node: Node) -> Status {
        match self {
            Sink::First(slot) => {
                *slot = Some(node);
                Status::Stop
            }
            Sink::All(nodes) => {
                nodes.push(node);
                Status::Ok
            }
            Sink::Matches { reference, found } => {
                if node == *reference {
                    *found = true;
                    Status::Stop
                } else {
                    Status::Ok
                }
            }
        }
    }
}

/// `lxb_selectors_cb_f` of one nesting level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Callback {
    /// The caller's, [`Sink`].
    Outer,
    /// `lxb_selectors_cb_ok`.
    Ok,
    /// `lxb_selectors_cb_not`.
    Not,
    /// `lxb_selectors_cb_nth_ok`.
    NthOk,
}

/// `lxb_selectors_state_cb_f`: the state functions of `selectors.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Find,
    FindForward,
    FoundCheck,
    FoundCheckForward,
    NotFound,
    NotFoundForward,
    AfterFind,
    AfterNot,
    AfterNthChild,
    NthChildFound,
}

/// A simple selector of a parsed list, `lxb_css_selector_t`, with the
/// place that gives its `prev`, `next` and `list->next`.
#[derive(Debug, Clone, Copy)]
struct Sel<'a> {
    list: &'a List,
    complex: usize,
    at: usize,
    selector: &'a Selector,
}

impl<'a> Sel<'a> {
    fn at(list: &'a List, complex: usize, at: usize) -> Option<Sel<'a>> {
        let selector = list.get(complex)?.selectors.get(at)?;
        Some(Sel {
            list,
            complex,
            at,
            selector,
        })
    }

    /// `list->first` of the complex selector at `complex`.
    fn first(list: &'a List, complex: usize) -> Option<Sel<'a>> {
        Sel::at(list, complex, 0)
    }

    /// `list->last` of the complex selector at `complex`.
    fn last(list: &'a List, complex: usize) -> Option<Sel<'a>> {
        let len = list.get(complex)?.selectors.len();
        Sel::at(list, complex, len.checked_sub(1)?)
    }

    fn prev(self) -> Option<Sel<'a>> {
        Sel::at(self.list, self.complex, self.at.checked_sub(1)?)
    }

    fn next(self) -> Option<Sel<'a>> {
        Sel::at(self.list, self.complex, self.at + 1)
    }

    fn combinator(self) -> Combinator {
        self.selector.combinator
    }

    /// Whether `list->next` is set: a complex selector follows.
    fn has_next_list(self) -> bool {
        self.complex + 1 < self.list.len()
    }
}

type EntryId = usize;
type NestedId = usize;

/// `lxb_selectors_entry_t` without the cached `id`, which is the
/// selector's name read again.
#[derive(Debug)]
struct Step<'a> {
    combinator: Combinator,
    selector: Sel<'a>,
    node: Option<Node>,
    next: Option<EntryId>,
    prev: Option<EntryId>,
    following: Option<EntryId>,
    nested: Option<NestedId>,
}

/// `lxb_selectors_nested_t`.
#[derive(Debug)]
struct Nested {
    entry: EntryId,
    return_state: State,
    callback: Callback,
    root: Option<Node>,
    parent: Option<NestedId>,
    first: EntryId,
    top: EntryId,
    index: usize,
    forward: bool,
}

/// `lxb_selectors_t` for one search: the entries and nesting levels
/// (`objs`, `nested`), the level being matched, the state, the options
/// and the status, with the caller's callback and whether `:blank` asked
/// for its `DOMException`.
struct Selectors<'a> {
    state: State,
    objs: Vec<Step<'a>>,
    nested: Vec<Nested>,
    current: NestedId,
    options: Options,
    status: Status,
    sink: Sink,
    blank: bool,
}

impl<'a> Selectors<'a> {
    /// `lxb_selectors_find`'s and `lxb_selectors_match_node`'s set-up:
    /// the outermost level over the list's first complex selector.
    fn new(list: &'a List, options: Options, sink: Sink) -> Option<Selectors<'a>> {
        let tail = Sel::last(list, 0)?;
        let mut s = Selectors {
            state: State::Find,
            objs: Vec::new(),
            nested: Vec::new(),
            current: 0,
            options,
            status: Status::Ok,
            sink,
            blank: false,
        };
        let entry = s.entry_make_first(tail);
        s.nested.push(Nested {
            entry,
            return_state: State::AfterFind,
            callback: Callback::Outer,
            root: None,
            parent: None,
            first: entry,
            top: entry,
            index: 0,
            forward: false,
        });
        Some(s)
    }

    fn alloc(&mut self, combinator: Combinator, selector: Sel<'a>, node: Option<Node>) -> EntryId {
        self.objs.push(Step {
            combinator,
            selector,
            node,
            next: None,
            prev: None,
            following: None,
            nested: None,
        });
        self.objs.len() - 1
    }

    fn cur(&self) -> &Nested {
        &self.nested[self.current]
    }

    fn cur_mut(&mut self) -> &mut Nested {
        &mut self.nested[self.current]
    }

    /// `lxb_selectors_state_entry_create`: the entries of the compound
    /// selector before `selector`'s, linked before `root`, the first of
    /// them carrying the combinator that joins the two.
    fn entry_create(&mut self, selector: Sel<'a>, root: EntryId, node: Option<Node>) -> EntryId {
        let combinator = selector.combinator();
        let mut last = root;
        let mut at = selector.prev();
        while let Some(s) = at {
            let entry = self.alloc(s.combinator(), s, node);
            if let Some(rp) = self.objs[root].prev {
                self.objs[rp].next = Some(entry);
                self.objs[entry].prev = Some(rp);
            }
            self.objs[entry].next = Some(root);
            self.objs[root].prev = Some(entry);
            last = entry;
            at = if s.combinator() == Combinator::Close {
                s.prev()
            } else {
                None
            };
        }
        self.objs[last].combinator = combinator;
        last
    }

    /// `lxb_selectors_state_entry_create_forward`.
    fn entry_create_forward(
        &mut self,
        selector: Sel<'a>,
        root: EntryId,
        node: Option<Node>,
    ) -> Option<EntryId> {
        let s = selector.next()?;
        let entry = self.alloc(s.combinator(), s, node);
        self.objs[entry].prev = Some(root);
        self.objs[root].next = Some(entry);
        Some(entry)
    }

    /// `lxb_selectors_entry_make_first`: the entries of the last compound
    /// selector, from its last simple selector back to its first.
    fn entry_make_first(&mut self, selector: Sel<'a>) -> EntryId {
        let mut prev: Option<EntryId> = None;
        let mut s = selector;
        loop {
            let entry = self.alloc(Combinator::Close, s, None);
            if let Some(p) = prev {
                self.objs[p].next = Some(entry);
                self.objs[entry].prev = Some(p);
            }
            match s.prev() {
                Some(before) if s.combinator() == Combinator::Close => {
                    prev = Some(entry);
                    s = before;
                }
                _ => return entry,
            }
        }
    }

    /// `lxb_selectors_descendant`.
    fn descendant(&mut self, entry: EntryId, node: Node) -> Option<Node> {
        let mut at = node.parent();
        while let Some(n) = at {
            if n.kind() == Kind::Element && self.matches(entry, n) {
                return Some(n);
            }
            at = n.parent();
        }
        None
    }

    /// `lxb_selectors_descendant_forward`: the elements after `node` in
    /// tree order within the level's root.
    fn descendant_forward(&mut self, entry: EntryId, node: Node) -> Option<Node> {
        let root = match self.objs[entry].prev {
            Some(p) => self.objs[p].node,
            None => self.cur().root,
        };
        let mut node = node;
        let mut descend = true;
        loop {
            match node.children() {
                Some(child) if descend => node = child,
                _ => {
                    while Some(node) != root && node.next().is_none() {
                        node = node.parent()?;
                    }
                    if Some(node) == root {
                        return None;
                    }
                    node = node.next()?;
                }
            }
            if node.kind() != Kind::Element {
                descend = false;
                continue;
            }
            descend = true;
            if self.matches(entry, node) {
                return Some(node);
            }
        }
    }

    /// `lxb_selectors_close`, `lxb_selectors_close_forward`.
    fn close(&mut self, entry: EntryId, node: Node) -> Option<Node> {
        self.matches(entry, node).then_some(node)
    }

    /// `lxb_selectors_child`.
    fn child(&mut self, entry: EntryId, node: Node) -> Option<Node> {
        let parent = node.parent()?;
        (parent.kind() == Kind::Element && self.matches(entry, parent)).then_some(parent)
    }

    /// `lxb_selectors_child_forward`.
    fn child_forward(&mut self, entry: EntryId, root: Node) -> Option<Node> {
        let start = match self.objs[entry].prev {
            Some(p) if self.objs[p].node == Some(root) => root.children(),
            None if self.cur().root == Some(root) => root.children(),
            _ => root.next(),
        };
        let mut at = start;
        while let Some(n) = at {
            if n.kind() == Kind::Element && self.matches(entry, n) {
                return Some(n);
            }
            at = n.next();
        }
        None
    }

    /// `lxb_selectors_sibling` (`forward` false) and
    /// `lxb_selectors_sibling_forward`: the nearest element sibling alone.
    fn sibling(&mut self, entry: EntryId, node: Node, forward: bool) -> Option<Node> {
        let step = |n: Node| if forward { n.next() } else { n.prev() };
        let mut at = step(node);
        while let Some(n) = at {
            if n.kind() == Kind::Element {
                return self.matches(entry, n).then_some(n);
            }
            at = step(n);
        }
        None
    }

    /// `lxb_selectors_following` (`forward` false) and
    /// `lxb_selectors_following_forward`: any element sibling.
    fn following(&mut self, entry: EntryId, node: Node, forward: bool) -> Option<Node> {
        let step = |n: Node| if forward { n.next() } else { n.prev() };
        let mut at = step(node);
        while let Some(n) = at {
            if n.kind() == Kind::Element && self.matches(entry, n) {
                return Some(n);
            }
            at = step(n);
        }
        None
    }

    fn switch_to_found_check(&mut self, nested: NestedId) {
        self.state = if self.nested[nested].forward {
            State::FoundCheckForward
        } else {
            State::FoundCheck
        };
    }

    fn switch_to_not_found(&mut self, nested: NestedId) {
        self.state = if self.nested[nested].forward {
            State::NotFoundForward
        } else {
            State::NotFound
        };
    }

    /// `lxb_selectors_tree`: every element below `root` in tree order,
    /// the children of any other node left out.
    fn tree(&mut self, root: Node) -> Status {
        let Some(mut node) = root.children() else {
            return Status::Ok;
        };
        loop {
            let element = node.kind() == Kind::Element;
            if element {
                match self.search(node) {
                    Status::Ok => {}
                    Status::Stop => break,
                    Status::Error => return Status::Error,
                }
            }
            match node.children() {
                Some(child) if element => node = child,
                _ => {
                    while node != root && node.next().is_none() {
                        match node.parent() {
                            Some(p) => node = p,
                            None => return Status::Ok,
                        }
                    }
                    if node == root {
                        break;
                    }
                    match node.next() {
                        Some(n) => node = n,
                        None => break,
                    }
                }
            }
        }
        Status::Ok
    }

    /// `lxb_selectors_run`: the states from the outermost level's first
    /// entry, standing on `node`, until one answers no entry.
    fn search(&mut self, node: Node) -> Status {
        let level = self.current;
        let entry = self.nested[level].entry;
        self.objs[entry].node = Some(node);
        self.nested[level].root = Some(node);
        self.state = State::Find;
        let mut at = Some(entry);
        while let Some(e) = at {
            at = match self.state {
                State::Find => self.state_find(e),
                State::FindForward => self.state_find_forward(e),
                State::FoundCheck => self.state_found_check(),
                State::FoundCheckForward => self.state_found_check_forward(),
                State::NotFound => self.state_not_found(),
                State::NotFoundForward => self.state_not_found_forward(e),
                State::AfterFind => self.state_after_find(),
                State::AfterNot => self.state_after_not(),
                State::AfterNthChild => self.state_after_nth_child(e),
                State::NthChildFound => self.state_nth_child_found(e),
            };
        }
        let top = self.nested[level].top;
        self.nested[level].first = top;
        self.nested[level].entry = top;
        self.status
    }

    /// `lxb_selectors_state_find`.
    fn state_find(&mut self, entry: EntryId) -> Option<EntryId> {
        self.state = State::FoundCheck;
        let node = self.objs[entry].node;
        let found = match (self.objs[entry].combinator, node) {
            (Combinator::Column, _) => {
                self.status = Status::Error;
                return None;
            }
            (_, None) => None,
            (Combinator::Descendant, Some(n)) => self.descendant(entry, n),
            (Combinator::Close, Some(n)) => self.close(entry, n),
            (Combinator::Child, Some(n)) => self.child(entry, n),
            (Combinator::Sibling, Some(n)) => self.sibling(entry, n, false),
            (Combinator::Following, Some(n)) => self.following(entry, n, false),
        };
        match found {
            None => self.state = State::NotFound,
            Some(n) => {
                let e = self.cur().entry;
                self.objs[e].node = Some(n);
            }
        }
        Some(self.cur().entry)
    }

    /// `lxb_selectors_state_find_forward`.
    fn state_find_forward(&mut self, entry: EntryId) -> Option<EntryId> {
        self.state = State::FoundCheckForward;
        let node = self.objs[entry].node;
        let found = match (self.objs[entry].combinator, node) {
            (Combinator::Column, _) => {
                self.status = Status::Error;
                return None;
            }
            (_, None) => None,
            (Combinator::Descendant, Some(n)) => self.descendant_forward(entry, n),
            (Combinator::Close, Some(n)) => self.close(entry, n),
            (Combinator::Child, Some(n)) => self.child_forward(entry, n),
            (Combinator::Sibling, Some(n)) => self.sibling(entry, n, true),
            (Combinator::Following, Some(n)) => self.following(entry, n, true),
        };
        match found {
            None => {
                let mut e = entry;
                loop {
                    loop {
                        match self.objs[e].prev {
                            None => return self.next_list_forward(e),
                            Some(p) => e = p,
                        }
                        if self.objs[e].combinator != Combinator::Close {
                            break;
                        }
                    }
                    if self.objs[e].combinator != Combinator::Sibling {
                        break;
                    }
                }
                self.cur_mut().entry = e;
                self.state = State::FindForward;
            }
            Some(n) => {
                let e = self.cur().entry;
                self.objs[e].node = Some(n);
            }
        }
        Some(self.cur().entry)
    }

    /// `lxb_selectors_done`.
    fn done(&mut self) -> Option<EntryId> {
        let parent = self.cur().parent?;
        self.current = parent;
        Some(self.cur().entry)
    }

    /// `lxb_selectors_exit`.
    fn exit(&mut self) -> Option<EntryId> {
        self.cur().parent?;
        self.state = self.cur().return_state;
        Some(self.cur().entry)
    }

    /// `lxb_selectors_state_found_check`.
    fn state_found_check(&mut self) -> Option<EntryId> {
        let entry = self.cur().entry;
        let node = self.objs[entry].node;
        let Some(prev) = self.objs[entry].prev else {
            let mut selector = self.objs[entry].selector;
            while selector.combinator() == Combinator::Close {
                match selector.prev() {
                    Some(p) => selector = p,
                    None => break,
                }
            }
            if selector.prev().is_none() {
                return self.state_found();
            }
            let prev = self.entry_create(selector, entry, node);
            self.cur_mut().entry = prev;
            self.state = State::Find;
            return Some(prev);
        };
        self.state = State::Find;
        self.cur_mut().entry = prev;
        self.objs[prev].node = node;
        Some(prev)
    }

    /// `lxb_selectors_state_found_check_forward`.
    fn state_found_check_forward(&mut self) -> Option<EntryId> {
        let entry = self.cur().entry;
        let node = self.objs[entry].node;
        let Some(next) = self.objs[entry].next else {
            let selector = self.objs[entry].selector;
            let Some(next) = self.entry_create_forward(selector, entry, node) else {
                return self.state_found_forward();
            };
            self.cur_mut().entry = next;
            self.state = State::FindForward;
            return Some(next);
        };
        self.state = State::FindForward;
        self.cur_mut().entry = next;
        self.objs[next].node = node;
        Some(next)
    }

    /// The level's callback on its root, `current->cb(current->root, ...)`.
    fn callback(&mut self) -> Status {
        let level = self.current;
        match self.nested[level].callback {
            Callback::Outer => match self.nested[level].root {
                Some(root) => self.sink.accept(root),
                None => Status::Ok,
            },
            Callback::Ok => {
                if let Some(parent) = self.nested[level].parent {
                    self.switch_to_found_check(parent);
                }
                Status::Ok
            }
            Callback::Not => {
                if let Some(parent) = self.nested[level].parent {
                    self.switch_to_not_found(parent);
                }
                Status::Ok
            }
            Callback::NthOk => {
                self.nested[level].index += 1;
                self.state = State::NthChildFound;
                Status::Ok
            }
        }
    }

    /// `lxb_selectors_state_found`.
    fn state_found(&mut self) -> Option<EntryId> {
        self.state = State::Find;
        self.status = self.callback();
        if !self.options.match_first && self.cur().parent.is_none() && self.status == Status::Ok {
            let first = self.cur().first;
            return self.next_list(first);
        }
        self.done()
    }

    /// `lxb_selectors_state_found_forward`.
    fn state_found_forward(&mut self) -> Option<EntryId> {
        self.state = State::FindForward;
        self.status = self.callback();
        if !self.options.match_first && self.cur().parent.is_none() && self.status == Status::Ok {
            let first = self.cur().first;
            return self.next_list_forward(first);
        }
        self.done()
    }

    /// `lxb_selectors_state_not_found`: back to the nearest descendant or
    /// following-sibling combinator on the right, whose search goes on
    /// from the node it stopped at.
    fn state_not_found(&mut self) -> Option<EntryId> {
        let mut entry = self.cur().entry;
        'try_next: loop {
            let Some(next) = self.objs[entry].next else {
                return self.next_list(entry);
            };
            entry = next;
            while self.objs[entry].combinator == Combinator::Close {
                match self.objs[entry].next {
                    Some(n) => entry = n,
                    None => continue 'try_next,
                }
            }
            if !matches!(
                self.objs[entry].combinator,
                Combinator::Sibling | Combinator::Child | Combinator::Close
            ) {
                break;
            }
        }
        self.cur_mut().entry = entry;
        self.state = State::Find;
        Some(entry)
    }

    /// `lxb_selectors_state_not_found_forward`.
    fn state_not_found_forward(&mut self, entry: EntryId) -> Option<EntryId> {
        let mut entry = entry;
        'try_next: loop {
            if self.objs[entry].prev.is_none() {
                return self.next_list_forward(entry);
            }
            while self.objs[entry].combinator == Combinator::Close {
                match self.objs[entry].prev {
                    Some(p) => entry = p,
                    None => continue 'try_next,
                }
            }
            if self.objs[entry].combinator == Combinator::Sibling {
                if let Some(p) = self.objs[entry].prev {
                    entry = p;
                }
                continue 'try_next;
            }
            break;
        }
        self.cur_mut().entry = entry;
        self.state = State::FindForward;
        Some(entry)
    }

    /// `lxb_selectors_next_list`: the next complex selector of the list,
    /// or out of the level.
    fn next_list(&mut self, entry: EntryId) -> Option<EntryId> {
        if !self.objs[entry].selector.has_next_list() {
            return self.exit();
        }
        self.state = State::Find;
        self.make_following(entry, false)
    }

    /// `lxb_selectors_next_list_forward`.
    fn next_list_forward(&mut self, entry: EntryId) -> Option<EntryId> {
        if !self.objs[entry].selector.has_next_list() {
            return self.exit();
        }
        self.state = State::FindForward;
        self.make_following(entry, true)
    }

    /// `lxb_selectors_make_following` and
    /// `lxb_selectors_make_following_forward`: the entry of the next
    /// complex selector, made once and kept on `entry`.
    fn make_following(&mut self, entry: EntryId, forward: bool) -> Option<EntryId> {
        let root = self.cur().root;
        let next = if let Some(f) = self.objs[entry].following {
            f
        } else {
            let selector = self.objs[entry].selector;
            let complex = selector.complex + 1;
            let next = if forward {
                let first = Sel::first(selector.list, complex)?;
                self.alloc(first.combinator(), first, None)
            } else {
                let last = Sel::last(selector.list, complex)?;
                self.entry_make_first(last)
            };
            self.objs[entry].following = Some(next);
            next
        };
        self.objs[next].node = root;
        self.cur_mut().first = next;
        self.cur_mut().entry = next;
        Some(next)
    }

    /// `lxb_selectors_state_after_find`.
    fn state_after_find(&mut self) -> Option<EntryId> {
        let parent = self.cur().parent?;
        self.current = parent;
        self.switch_to_not_found(parent);
        Some(self.cur().entry)
    }

    /// `lxb_selectors_state_after_not`.
    fn state_after_not(&mut self) -> Option<EntryId> {
        let parent = self.cur().parent?;
        self.current = parent;
        self.switch_to_found_check(parent);
        Some(self.cur().entry)
    }

    /// The `An+B` and direction of the `:nth-child`/`:nth-last-child`
    /// function `entry` stands for.
    fn nth_of(&self, entry: EntryId) -> Option<(&'a Anb, bool)> {
        match &self.objs[entry].selector.selector.kind {
            SelectorKind::PseudoClassFunction(PseudoClassFunction::NthChild(anb)) => {
                Some((anb, false))
            }
            SelectorKind::PseudoClassFunction(PseudoClassFunction::NthLastChild(anb)) => {
                Some((anb, true))
            }
            _ => None,
        }
    }

    /// `lxb_selectors_state_nth_child_done`.
    fn nth_child_done(&mut self, anb: &Anb, index: usize) -> EntryId {
        let level = self.current;
        if anb_calc(anb, index) {
            self.switch_to_found_check(level);
        } else {
            self.switch_to_not_found(level);
        }
        self.cur().entry
    }

    /// `lxb_selectors_state_after_nth_child`: `S` did not match the
    /// sibling, so on to the one before (or after) it.
    fn state_after_nth_child(&mut self, entry: EntryId) -> Option<EntryId> {
        let level = self.current;
        let parent = self.nested[level].parent?;
        if self.nested[level].index == 0 {
            self.current = parent;
            self.switch_to_not_found(parent);
            return Some(self.cur().entry);
        }
        let (anb, last) = self.nth_of(self.nested[parent].entry)?;
        let node = self.nested[level]
            .root
            .and_then(|r| nth_child_node(r, last));
        let Some(node) = node else {
            let index = self.nested[level].index;
            self.current = parent;
            return Some(self.nth_child_done(anb, index));
        };
        self.nested[level].root = Some(node);
        let e = self.nested[level].entry;
        self.objs[e].node = Some(node);
        self.state = State::Find;
        Some(entry)
    }

    /// `lxb_selectors_state_nth_child_found`: `S` matched, counted, on to
    /// the next sibling from the entry the match stopped at.
    fn state_nth_child_found(&mut self, entry: EntryId) -> Option<EntryId> {
        let level = self.objs[entry].nested?;
        let (anb, last) = self.nth_of(entry)?;
        let node = self.nested[level]
            .root
            .and_then(|r| nth_child_node(r, last));
        let Some(node) = node else {
            let index = self.nested[level].index;
            return Some(self.nth_child_done(anb, index));
        };
        self.nested[level].root = Some(node);
        let e = self.nested[level].entry;
        self.objs[e].node = Some(node);
        self.current = level;
        self.state = State::Find;
        Some(e)
    }

    /// `lxb_selectors_match`: one simple selector against one element.
    fn matches(&mut self, entry: EntryId, node: Node) -> bool {
        let selector = self.objs[entry].selector.selector;
        let quirks = self.options.quirks;
        match &selector.kind {
            SelectorKind::Any => true,
            SelectorKind::Element => cmp_local_name(node, &selector.name),
            SelectorKind::Id => match_id(&selector.name, node, quirks),
            SelectorKind::Class => {
                let Some(attr) = adapted_attr(node, b"class") else {
                    return false;
                };
                let value = attr.attr_value();
                !value.is_empty() && match_class(&value, &selector.name, quirks)
            }
            SelectorKind::Attribute(m) => match_attribute(selector, m.as_ref(), node),
            SelectorKind::PseudoClass(p) => self.pseudo_class(*p, node),
            SelectorKind::PseudoClassFunction(f) => self.pseudo_class_function(f, node),
            SelectorKind::PseudoElement(_) | SelectorKind::PseudoElementFunction => false,
        }
    }

    /// `lxb_selectors_pseudo_class`.
    fn pseudo_class(&mut self, pseudo: PseudoClass, node: Node) -> bool {
        let html = ns_is(node, HTML_NS);
        match pseudo {
            PseudoClass::AnyLink | PseudoClass::Link => {
                html && is_named(node, &[b"a", b"area"]) && has_attr(node, b"href")
            }
            PseudoClass::Blank => {
                self.blank = true;
                false
            }
            PseudoClass::Checked => {
                if !html {
                    return false;
                }
                if is_named(node, &[b"input"]) {
                    let Some(attr) = adapted_attr(node, b"type") else {
                        return false;
                    };
                    let value = attr.attr_value();
                    (value.eq_ignore_ascii_case(b"checkbox")
                        || value.eq_ignore_ascii_case(b"radio"))
                        && has_attr(node, b"checked")
                } else if is_named(node, &[b"option"]) {
                    has_attr(node, b"selected")
                } else {
                    false
                }
            }
            PseudoClass::Disabled => disabled(node),
            PseudoClass::Enabled => !disabled(node),
            PseudoClass::Empty => {
                let mut at = node.children();
                while let Some(n) = at {
                    if !matches!(n.kind(), Kind::Comment | Kind::Pi) {
                        return false;
                    }
                    at = n.next();
                }
                true
            }
            PseudoClass::FirstChild => first_child(node),
            PseudoClass::FirstOfType => first_of_type(node),
            PseudoClass::LastChild => last_child(node),
            PseudoClass::LastOfType => last_of_type(node),
            PseudoClass::OnlyChild => first_child(node) && last_child(node),
            PseudoClass::OnlyOfType => first_of_type(node) && last_of_type(node),
            PseudoClass::Optional => {
                html && is_named(node, &[b"input", b"select", b"textarea"])
                    && !has_attr(node, b"required")
            }
            PseudoClass::Required => {
                html && is_named(node, &[b"input", b"select", b"textarea"])
                    && has_attr(node, b"required")
            }
            PseudoClass::PlaceholderShown => {
                html && is_named(node, &[b"input", b"textarea"]) && has_attr(node, b"placeholder")
            }
            PseudoClass::ReadOnly => !read_write(node),
            PseudoClass::ReadWrite => read_write(node),
            PseudoClass::Root => node.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    Kind::Fragment | Kind::Document | Kind::HtmlDocument
                )
            }),
            _ => false,
        }
    }

    /// `lxb_selectors_nested_make`: the level `entry`'s pseudo-class
    /// function opens on `node`, made once and entered afresh each time.
    fn nested_make(&mut self, node: Node, selector: Sel<'a>, forward: bool) -> NestedId {
        let entry = self.cur().entry;
        self.objs[entry].node = Some(node);
        let level = if let Some(level) = self.objs[entry].nested {
            level
        } else {
            let top = if forward {
                self.alloc(selector.combinator(), selector, None)
            } else {
                self.entry_make_first(selector)
            };
            self.nested.push(Nested {
                entry: top,
                return_state: State::AfterFind,
                callback: Callback::Ok,
                root: None,
                parent: Some(self.current),
                first: top,
                top,
                index: 0,
                forward,
            });
            let level = self.nested.len() - 1;
            self.objs[entry].nested = Some(level);
            level
        };
        self.current = level;
        let top = self.nested[level].top;
        self.nested[level].entry = top;
        self.nested[level].first = top;
        self.nested[level].root = Some(node);
        level
    }

    /// Open a level over `list` whose callback and return state are
    /// given, the states going on inside it from `state`.
    fn open(
        &mut self,
        node: Node,
        selector: Option<Sel<'a>>,
        forward: bool,
        callback: Callback,
        return_state: State,
    ) -> bool {
        let Some(selector) = selector else {
            return false;
        };
        let level = self.nested_make(node, selector, forward);
        self.nested[level].callback = callback;
        self.nested[level].return_state = return_state;
        self.state = if forward {
            State::FindForward
        } else {
            State::Find
        };
        true
    }

    /// `lxb_selectors_pseudo_class_function`.
    fn pseudo_class_function(&mut self, function: &'a PseudoClassFunction, node: Node) -> bool {
        match function {
            PseudoClassFunction::Has(list) => self.open(
                node,
                Sel::first(list, 0),
                true,
                Callback::Ok,
                State::AfterFind,
            ),
            PseudoClassFunction::Current(list)
            | PseudoClassFunction::Is(list)
            | PseudoClassFunction::Where(list) => self.open(
                node,
                Sel::last(list, 0),
                false,
                Callback::Ok,
                State::AfterFind,
            ),
            PseudoClassFunction::Not(list) => self.open(
                node,
                Sel::last(list, 0),
                false,
                Callback::Not,
                State::AfterNot,
            ),
            PseudoClassFunction::NthChild(anb) | PseudoClassFunction::NthLastChild(anb) => {
                let last = matches!(function, PseudoClassFunction::NthLastChild(_));
                if let Some(of) = &anb.of {
                    let opened = self.open(
                        node,
                        Sel::last(of, 0),
                        false,
                        Callback::NthOk,
                        State::AfterNthChild,
                    );
                    if opened {
                        self.cur_mut().index = 0;
                    }
                    return opened;
                }
                let mut index = 0;
                let mut at = Some(node);
                while let Some(n) = at {
                    if n.kind() == Kind::Element {
                        index += 1;
                    }
                    at = if last { n.next() } else { n.prev() };
                }
                anb_calc(anb, index)
            }
            PseudoClassFunction::NthOfType(anb) | PseudoClassFunction::NthLastOfType(anb) => {
                let last = matches!(function, PseudoClassFunction::NthLastOfType(_));
                let name = node.name();
                let mut index = 0;
                let mut at = Some(node);
                while let Some(n) = at {
                    if n.kind() == Kind::Element && n.name() == name && cmp_ns(n, node) {
                        index += 1;
                    }
                    at = if last { n.next() } else { n.prev() };
                }
                anb_calc(anb, index)
            }
            _ => false,
        }
    }
}

/// `lxb_selectors_state_nth_child_node`: the element sibling before
/// `node` (`:nth-child`) or after it (`:nth-last-child`).
fn nth_child_node(node: Node, last: bool) -> Option<Node> {
    let step = |n: Node| if last { n.next() } else { n.prev() };
    let mut at = step(node);
    while let Some(n) = at {
        if n.kind() == Kind::Element {
            return Some(n);
        }
        at = step(n);
    }
    None
}

/// `lxb_selectors_anb_calc`: whether `index` (from 1) is `An+B` for some
/// `n >= 0`, in doubles as lexbor computes it.
fn anb_calc(anb: &Anb, index: usize) -> bool {
    if anb.a == 0 {
        return usize::try_from(anb.b).is_ok_and(|b| b == index);
    }
    #[allow(clippy::cast_precision_loss)] // lexbor's own conversion to double
    let num = (index as f64 - anb.b as f64) / anb.a as f64;
    num >= 0.0 && num - num.trunc() == 0.0
}

/// `lxb_selectors_adapted_cmp_ns`: the same namespace declaration, or two
/// with one URI.
fn cmp_ns(a: Node, b: Node) -> bool {
    let (x, y) = (a.ns(), b.ns());
    x == y
        || match (x, y) {
            (Some(x), Some(y)) => x.href() == y.href(),
            _ => false,
        }
}

/// `lxb_selectors_adapted_cmp_local_name_id`: an HTML element of an HTML
/// document compares its name with the selector's lowercased, any other
/// byte for byte.
fn cmp_local_name(node: Node, name: &[u8]) -> bool {
    let local = node.name();
    if html_in_html_document(node) {
        lowered_equals(name, &local)
    } else {
        local == name
    }
}

/// `lxb_selectors_str_cmp_loright`: `lhs` lowercased equals `rhs`.
fn lowered_equals(lhs: &[u8], rhs: &[u8]) -> bool {
    lhs.len() == rhs.len()
        && lhs
            .iter()
            .zip(rhs)
            .all(|(l, r)| l.to_ascii_lowercase() == *r)
}

/// `lxb_selectors_cmp_html_name_lit` over a few names.
fn is_named(node: Node, names: &[&[u8]]) -> bool {
    let local = node.name();
    names.contains(&local.as_slice())
}

/// `lxb_selectors_adapted_attr`: the attribute `name` names on `node`,
/// matched ASCII case-insensitively on an HTML element of an HTML
/// document, none where it has a namespace.
fn adapted_attr(node: Node, name: &[u8]) -> Option<Node> {
    let attr = if html_in_html_document(node) {
        let mut at = node.properties();
        let mut found = None;
        while let Some(a) = at {
            if lowered_equals(name, &a.name()) {
                found = Some(a);
                break;
            }
            at = a.next();
        }
        found
    } else {
        node.has_prop(name)
    }?;
    attr.ns().is_none().then_some(attr)
}

/// `lxb_selectors_adapted_has_attr`.
fn has_attr(node: Node, name: &[u8]) -> bool {
    adapted_attr(node, name).is_some()
}

/// `lexbor_str_data_ncasecmp` over equal lengths, or `memcmp`.
fn same(a: &[u8], b: &[u8], insensitive: bool) -> bool {
    if insensitive {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// `lxb_selectors_match_id`.
fn match_id(name: &[u8], node: Node, quirks: bool) -> bool {
    let Some(attr) = adapted_attr(node, b"id") else {
        return false;
    };
    let value = attr.attr_value();
    value.len() == name.len() && same(&value, name, quirks)
}

/// `lexbor_utils_whitespace`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r')
}

/// `lxb_selectors_match_class`: `src` one of the whitespace-separated
/// words of `target`, with lexbor's early exit when too few bytes are
/// left.
fn match_class(target: &[u8], src: &[u8], insensitive: bool) -> bool {
    if target.len() < src.len() {
        return false;
    }
    let mut pos = 0;
    for (i, &c) in target.iter().enumerate() {
        if is_space(c) {
            if i - pos == src.len()
                && target
                    .get(pos..i)
                    .is_some_and(|w| same(w, src, insensitive))
            {
                return true;
            }
            if target.len() - i < src.len() {
                return false;
            }
            pos = i + 1;
        }
    }
    target.len() - pos == src.len()
        && !src.is_empty()
        && target.get(pos..).is_some_and(|w| same(w, src, insensitive))
}

/// `lxb_selectors_is_lowercased_html_attrib_name`: the attributes whose
/// values HTML compares ASCII case-insensitively.
fn lowercased_html_attribute(name: &[u8]) -> bool {
    const NAMES: &[&[u8]] = &[
        b"accept",
        b"accept-charset",
        b"align",
        b"alink",
        b"axis",
        b"bgcolor",
        b"charset",
        b"checked",
        b"clear",
        b"codetype",
        b"color",
        b"compact",
        b"declare",
        b"defer",
        b"dir",
        b"direction",
        b"disabled",
        b"enctype",
        b"face",
        b"frame",
        b"hreflang",
        b"http-equiv",
        b"lang",
        b"language",
        b"link",
        b"media",
        b"method",
        b"multiple",
        b"nohref",
        b"noresize",
        b"noshade",
        b"nowrap",
        b"readonly",
        b"rel",
        b"rev",
        b"rules",
        b"scope",
        b"scrolling",
        b"selected",
        b"shape",
        b"target",
        b"text",
        b"type",
        b"valign",
        b"valuetype",
        b"vlink",
    ];
    NAMES.iter().any(|n| lowered_equals(name, n))
}

/// `lxb_selectors_match_attribute` and
/// `lxb_selectors_match_attribute_value`.
fn match_attribute(selector: &Selector, m: Option<&AttributeMatch>, node: Node) -> bool {
    let Some(attr) = adapted_attr(node, &selector.name) else {
        return false;
    };
    let Some(m) = m else {
        return true;
    };
    let trg = attr.attr_value();
    let src = m.value.as_slice();
    let ins = m.modifier == Modifier::I
        || (lowercased_html_attribute(&selector.name) && html_in_html_document(node));
    match m.matcher {
        Match::Equal => trg.len() == src.len() && same(&trg, src, ins),
        Match::Include => match_class(&trg, src, ins),
        Match::Dash => {
            if trg.len() == src.len() {
                return same(&trg, src, ins);
            }
            trg.len() > src.len()
                && trg.get(..src.len()).is_some_and(|p| same(p, src, ins))
                && trg.get(src.len()) == Some(&b'-')
        }
        Match::Prefix => !src.is_empty() && trg.get(..src.len()).is_some_and(|p| same(p, src, ins)),
        Match::Suffix => {
            !src.is_empty()
                && trg.len() >= src.len()
                && trg
                    .get(trg.len() - src.len()..)
                    .is_some_and(|p| same(p, src, ins))
        }
        Match::Substring => !src.is_empty() && trg.windows(src.len()).any(|w| same(w, src, ins)),
    }
}

/// `lxb_selectors_pseudo_class_disabled`: a disabled form control, or a
/// fieldset inside a disabled fieldset but outside its first legend.
fn disabled(node: Node) -> bool {
    if !ns_is(node, HTML_NS) {
        return false;
    }
    if has_attr(node, b"disabled")
        && is_named(
            node,
            &[
                b"button",
                b"input",
                b"select",
                b"textarea",
                b"optgroup",
                b"fieldset",
            ],
        )
    {
        return true;
    }
    if !is_named(node, &[b"fieldset"]) {
        return false;
    }
    let fieldset = node;
    let mut at = node.parent();
    while let Some(ancestor) = at.filter(|n| n.kind() == Kind::Element) {
        if ns_is(ancestor, HTML_NS)
            && is_named(ancestor, &[b"fieldset"])
            && has_attr(ancestor, b"disabled")
        {
            let mut search = ancestor.children();
            while let Some(child) = search {
                if child.kind() == Kind::Element
                    && ns_is(child, HTML_NS)
                    && is_named(child, &[b"legend"])
                {
                    let mut inner = Some(fieldset);
                    while let Some(i) = inner {
                        if i == child {
                            return false;
                        }
                        inner = i.parent();
                    }
                    return true;
                }
                search = child.next();
            }
        }
        at = ancestor.parent();
    }
    false
}

fn first_child(node: Node) -> bool {
    nth_child_node(node, false).is_none()
}

fn last_child(node: Node) -> bool {
    nth_child_node(node, true).is_none()
}

/// `lxb_selectors_pseudo_class_first_of_type` (`last` false) and
/// `lxb_selectors_pseudo_class_last_of_type`.
fn alone_of_type(node: Node, last: bool) -> bool {
    let name = node.name();
    let step = |n: Node| if last { n.next() } else { n.prev() };
    let mut at = step(node);
    while let Some(n) = at {
        if n.kind() == Kind::Element && n.name() == name && cmp_ns(n, node) {
            return false;
        }
        at = step(n);
    }
    true
}

fn first_of_type(node: Node) -> bool {
    alone_of_type(node, false)
}

fn last_of_type(node: Node) -> bool {
    alone_of_type(node, true)
}

/// `lxb_selectors_pseudo_class_read_write`.
fn read_write(node: Node) -> bool {
    if !ns_is(node, HTML_NS) {
        return false;
    }
    if is_named(node, &[b"input", b"textarea"]) {
        return !has_attr(node, b"readonly") && !has_attr(node, b"disabled");
    }
    adapted_attr(node, b"contenteditable").is_some_and(|a| a.attr_value() != b"false")
}

/// What one search answered: the status, the callback's nodes and
/// whether `:blank` was met.
struct Answer {
    status: Status,
    sink: Sink,
    blank: bool,
}

/// `lxb_selectors_find`: the elements below `root` in tree order, each
/// once per complex selector it matches (once in all with
/// `match_first`).
fn find(root: Node, list: &List, options: Options, sink: Sink) -> Answer {
    let Some(mut s) = Selectors::new(list, options, sink) else {
        return Answer {
            status: Status::Ok,
            sink: Sink::All(Vec::new()),
            blank: false,
        };
    };
    let status = s.tree(root);
    Answer {
        status,
        sink: s.sink,
        blank: s.blank,
    }
}

/// `lxb_selectors_match_node`: `node` alone, which only an element can
/// match.
fn match_node(node: Node, list: &List, options: Options) -> Answer {
    let sink = Sink::Matches {
        reference: node,
        found: false,
    };
    if node.kind() != Kind::Element {
        return Answer {
            status: Status::Ok,
            sink,
            blank: false,
        };
    }
    let Some(mut s) = Selectors::new(list, options, sink) else {
        return Answer {
            status: Status::Ok,
            sink: Sink::All(Vec::new()),
            blank: false,
        };
    };
    let status = s.search(node);
    Answer {
        status,
        sink: s.sink,
        blank: s.blank,
    }
}

/// `dom_parse_selector`: the list, parsed under a lend so lexbor's parser
/// is the request's memory while it works, or php's `DOMException`
/// naming lexbor's first complaint.
fn parse(req: &mut Request, text: &[u8]) -> Result<List, Thrown> {
    php_ext_lexbor::ffi::lent(req, |lent| List::parse(lent, text))?.map_err(|refused| {
        let message = match refused.message {
            Some(m) => format!("Invalid selector ({})", String::from_utf8_lossy(&m)),
            None => "Invalid selector".to_owned(),
        };
        crate::dom_throw_message(req, Code::Syntax, &message)
    })
}

/// `dom_quirks_opt`: quirks and limited quirks both set the option.
fn quirks(req: &Request, object: PhpObject) -> bool {
    dom(req, object).is_some_and(|d| d.obj.quirks() != Quirks::NoQuirks)
}

/// `dom_check_css_execution_status`, with `:blank`'s `DOMException`,
/// which the C throws from inside the match and this throws once it ends.
fn raise(req: &mut Request, status: Status, blank: bool) -> Result<(), Thrown> {
    if status == Status::Error {
        let ce = Throwables::of(req)?;
        return Err(argument_error(
            req,
            ce.value_error,
            1,
            "contains an unsupported selector",
        ));
    }
    if blank {
        return Err(crate::dom_throw_message(
            req,
            Code::NotSupported,
            ":blank selector is not implemented because CSSWG has not yet decided its semantics (https://github.com/w3c/csswg-drafts/issues/1967)",
        ));
    }
    Ok(())
}

/// The receiver's node, the selector list parsed and the matching options,
/// `php_dom_dispatch_query_selector`'s and `dom_element_matches`' start.
fn selector_call(
    req: &mut Request,
    this: PhpObject,
    text: &[u8],
) -> Result<(Node, List, Options), Thrown> {
    let node = fetch(req, this)?;
    let list = parse(req, text)?;
    let options = Options {
        match_first: true,
        quirks: quirks(req, this),
    };
    Ok((node, list, options))
}

/// `dom_parent_node_query_selector`: the first element below the node in
/// tree order that the list matches.
pub(crate) fn query_selector(
    req: &mut Request,
    this: PhpObject,
    selectors: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let (node, list, options) = selector_call(req, this, selectors)?;
    let answer = find(node, &list, options, Sink::First(None));
    raise(req, answer.status, answer.blank)?;
    match answer.sink {
        Sink::First(Some(found)) => object_for(req, found, Some(this)),
        _ => Ok(Owner::null()),
    }
}

/// `dom_parent_node_query_selector_all`: every element below the node in
/// tree order, once for each complex selector of the list it matches.
pub(crate) fn query_selector_all(
    req: &mut Request,
    this: PhpObject,
    selectors: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let (node, list, options) = selector_call(req, this, selectors)?;
    let options = Options {
        match_first: false,
        ..options
    };
    let answer = find(node, &list, options, Sink::All(Vec::new()));
    raise(req, answer.status, answer.blank)?;
    let nodes = match answer.sink {
        Sink::All(nodes) => nodes,
        _ => Vec::new(),
    };
    let mut array = PhpArray::new(req)?;
    for n in nodes {
        let v = match object_for(req, n, Some(this)) {
            Ok(v) => v,
            Err(thrown) => {
                array.give_up(req);
                return Err(thrown);
            }
        };
        array.append(req, v)?;
    }
    crate::node_set(req, "Dom\\NodeList", array)
}

/// `dom_element_matches`: whether the list matches the element.
pub(crate) fn matches(
    req: &mut Request,
    this: PhpObject,
    selectors: &[u8],
) -> Result<bool, Thrown> {
    let (node, list, options) = selector_call(req, this, selectors)?;
    let answer = match_node(node, &list, options);
    raise(req, answer.status, answer.blank)?;
    Ok(matches!(answer.sink, Sink::Matches { found: true, .. }))
}

/// `dom_element_closest`: the element itself or its nearest ancestor that
/// the list matches.
pub(crate) fn closest(
    req: &mut Request,
    this: PhpObject,
    selectors: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let (node, list, options) = selector_call(req, this, selectors)?;
    let mut blank = false;
    let mut at = Some(node);
    while let Some(current) = at {
        let answer = match_node(current, &list, options);
        blank |= answer.blank;
        if answer.status == Status::Error {
            raise(req, answer.status, blank)?;
        }
        if matches!(answer.sink, Sink::Matches { found: true, .. }) {
            raise(req, Status::Ok, blank)?;
            return object_for(req, current, Some(this));
        }
        at = current.parent();
    }
    raise(req, Status::Ok, blank)?;
    Ok(Owner::null())
}
