//! The object model of `php_dom.c`: one payload for every dom class
//! ([`Dom`], `dom_object`), the handler tables php installs on them, the
//! object a libxml node answers as (`php_dom_create_object`), and the
//! helpers every native reads its receiver and arguments through.

use php_ext::abi::{CloneObj, ObjectHandlers, PropertyPlace, PropertyRead, PropertyTest, Std};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};
use php_ext_libxml::ffi::NodeObject;

use crate::map::Map;
use crate::xpath::XPathState;
use crate::{
    debug_info, dom_mut, has_property, property_place, read_property, unset_property,
    write_property,
};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005). php's `zend_dom_globals`.
#[derive(Default)]
pub(crate) struct State {
    /// `DOM_G(suppress_warnings)`: set while a dumper reads every
    /// property, so a deprecated one raises nothing.
    pub(crate) suppress_warnings: bool,
}

/// `dom_object`: the libxml half every dom object has, and what a list, a
/// map, an `XPath` object or a namespace node keeps beside it.
#[derive(Default)]
pub(crate) struct Dom {
    /// `ptr` for a node, and `document`: php's `php_libxml_node_object`.
    pub(crate) obj: NodeObject,
    pub(crate) extra: Extra,
}

/// What `dom_object::ptr` is beyond a node, one variant per object kind
/// that has more.
#[derive(Default)]
pub(crate) enum Extra {
    /// A node object, or a `DOMImplementation`.
    #[default]
    None,
    /// `dom_nnodemap_object`: a `DOMNodeList` or a `DOMNamedNodeMap`.
    Map(Box<Map>),
    /// `dom_xpath_object`'s own fields.
    XPath(Box<XPathState>),
    /// `dom_object_namespace_node::parent_intern`, the object's owner of
    /// its element's object.
    NamespaceParent(Owner<PhpObject>),
    /// `dom_token_list_object`'s own fields.
    TokenList(Box<crate::token_list::TokenSet>),
}

// ---- class names and lookups

// ---- DOMException and php_dom_throw_error

/// `dom_exception_code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    IndexSize = 1,
    StringSize = 2,
    HierarchyRequest = 3,
    WrongDocument = 4,
    InvalidCharacter = 5,
    NoDataAllowed = 6,
    NoModificationAllowed = 7,
    NotFound = 8,
    NotSupported = 9,
    InuseAttribute = 10,
    InvalidState = 11,
    Syntax = 12,
    InvalidModification = 13,
    Namespace = 14,
    InvalidAccess = 15,
    Validation = 16,
}

impl Code {
    /// `php_dom_throw_error`'s message.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Code::IndexSize => "Index Size Error",
            Code::StringSize => "DOM String Size Error",
            Code::HierarchyRequest => "Hierarchy Request Error",
            Code::WrongDocument => "Wrong Document Error",
            Code::InvalidCharacter => "Invalid Character Error",
            Code::NoDataAllowed => "No Data Allowed Error",
            Code::NoModificationAllowed => "No Modification Allowed Error",
            Code::NotFound => "Not Found Error",
            Code::NotSupported => "Not Supported Error",
            Code::InuseAttribute => "Inuse Attribute Error",
            Code::InvalidState => "Invalid State Error",
            Code::Syntax => "Syntax Error",
            Code::InvalidModification => "Invalid Modification Error",
            Code::Namespace => "Namespace Error",
            Code::InvalidAccess => "Invalid Access Error",
            Code::Validation => "Validation Error",
        }
    }
}

// ---- receivers and arguments

// ---- php_dom_create_object

// ---- the property handlers php installs on every dom object

/// The table every dom node class reaches its objects through:
/// `dom_object_handlers`, and the copies of it `PHP_MINIT_FUNCTION(dom)`
/// makes with another `clone_obj`.
pub(crate) struct NodeHandlers {
    clone: Option<CloneObj>,
}

/// The handlers php gives a node object, `dom_object_handlers`.
pub(crate) static NODE_HANDLERS: NodeHandlers = NodeHandlers {
    clone: Some(crate::clone_object),
};

/// `dom_modern_domimplementation_object_handlers`: `[SameObject]`, so no
/// clone.
pub(crate) static IMPLEMENTATION_HANDLERS: NodeHandlers = NodeHandlers { clone: None };

/// `dom_modern_element_object_handlers`: a clone drops the cached
/// `$classList` and `$children` (`dom_modern_element_clone_obj`).
pub(crate) static ELEMENT_HANDLERS: NodeHandlers = NodeHandlers {
    clone: Some(crate::modern::clone_element),
};

/// `dom_unset_children_property_object_handlers`: a clone drops the cached
/// `$children` (`dom_clone_obj_unset_children_property`).
pub(crate) static PARENT_HANDLERS: NodeHandlers = NodeHandlers {
    clone: Some(crate::modern::clone_parent),
};

impl ObjectHandlers for NodeHandlers {
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        read_property(req, object, name, read)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        property_place(req, object, name)
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        has_property(req, object, name, test)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        unset_property(req, object, name)
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        debug_info(req, object)
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        self.clone
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let parent = dom_mut(req, object).and_then(|d| match std::mem::take(&mut d.extra) {
            Extra::NamespaceParent(p) => Some(p),
            _ => None,
        });
        // `dom_object_namespace_node_free_storage`: the parent given up
        // first, then `dom_objects_free_storage`, which is
        // `zend_object_std_dtor` and then the node let go of.
        if let Some(parent) = parent {
            parent.give_up(req);
        }
        Std.free_obj(req, object);
        crate::release_libxml(req, object);
    }
}
