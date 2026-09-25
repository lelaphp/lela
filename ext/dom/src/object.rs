//! The object model of `php_dom.c`: one payload for every dom class
//! ([`Dom`], `dom_object`), the handler tables php installs on them, the
//! object a libxml node answers as (`php_dom_create_object`), and the
//! helpers every native reads its receiver and arguments through.

use php_ext::abi::{CloneObj, ObjectHandlers, PropertyRead, PropertyTest};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpObject, PhpString, Value};
use php_ext_libxml::ffi::NodeObject;

use crate::map::Map;
use crate::xpath::XPathState;
use crate::{
    debug_info, dom_mut, has_property, property_slot, read_property, unset_property, write_property,
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
    /// `dom_object_namespace_node::parent_intern`, one owner held.
    NamespaceParent(PhpObject),
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
    /// Every code, in the order `php_dom_arginfo.h` registers its
    /// constant.
    pub(crate) const ALL: [Code; 16] = [
        Code::IndexSize,
        Code::StringSize,
        Code::HierarchyRequest,
        Code::WrongDocument,
        Code::InvalidCharacter,
        Code::NoDataAllowed,
        Code::NoModificationAllowed,
        Code::NotFound,
        Code::NotSupported,
        Code::InuseAttribute,
        Code::InvalidState,
        Code::Syntax,
        Code::InvalidModification,
        Code::Namespace,
        Code::InvalidAccess,
        Code::Validation,
    ];

    /// The constant php registers the code as.
    pub(crate) fn constant(self) -> &'static [u8] {
        match self {
            Code::IndexSize => b"DOM_INDEX_SIZE_ERR",
            Code::StringSize => b"DOMSTRING_SIZE_ERR",
            Code::HierarchyRequest => b"DOM_HIERARCHY_REQUEST_ERR",
            Code::WrongDocument => b"DOM_WRONG_DOCUMENT_ERR",
            Code::InvalidCharacter => b"DOM_INVALID_CHARACTER_ERR",
            Code::NoDataAllowed => b"DOM_NO_DATA_ALLOWED_ERR",
            Code::NoModificationAllowed => b"DOM_NO_MODIFICATION_ALLOWED_ERR",
            Code::NotFound => b"DOM_NOT_FOUND_ERR",
            Code::NotSupported => b"DOM_NOT_SUPPORTED_ERR",
            Code::InuseAttribute => b"DOM_INUSE_ATTRIBUTE_ERR",
            Code::InvalidState => b"DOM_INVALID_STATE_ERR",
            Code::Syntax => b"DOM_SYNTAX_ERR",
            Code::InvalidModification => b"DOM_INVALID_MODIFICATION_ERR",
            Code::Namespace => b"DOM_NAMESPACE_ERR",
            Code::InvalidAccess => b"DOM_INVALID_ACCESS_ERR",
            Code::Validation => b"DOM_VALIDATION_ERR",
        }
    }

    /// The constant the `Dom\` namespace registers the code as; `Dom\`
    /// has no `INVALID_ACCESS_ERR`.
    pub(crate) fn modern_constant(self) -> Option<&'static [u8]> {
        Some(match self {
            Code::IndexSize => b"Dom\\INDEX_SIZE_ERR",
            Code::StringSize => b"Dom\\STRING_SIZE_ERR",
            Code::HierarchyRequest => b"Dom\\HIERARCHY_REQUEST_ERR",
            Code::WrongDocument => b"Dom\\WRONG_DOCUMENT_ERR",
            Code::InvalidCharacter => b"Dom\\INVALID_CHARACTER_ERR",
            Code::NoDataAllowed => b"Dom\\NO_DATA_ALLOWED_ERR",
            Code::NoModificationAllowed => b"Dom\\NO_MODIFICATION_ALLOWED_ERR",
            Code::NotFound => b"Dom\\NOT_FOUND_ERR",
            Code::NotSupported => b"Dom\\NOT_SUPPORTED_ERR",
            Code::InuseAttribute => b"Dom\\INUSE_ATTRIBUTE_ERR",
            Code::InvalidState => b"Dom\\INVALID_STATE_ERR",
            Code::Syntax => b"Dom\\SYNTAX_ERR",
            Code::InvalidModification => b"Dom\\INVALID_MODIFICATION_ERR",
            Code::Namespace => b"Dom\\NAMESPACE_ERR",
            Code::InvalidAccess => return None,
            Code::Validation => b"Dom\\VALIDATION_ERR",
        })
    }

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
    ) -> Result<Value, Thrown> {
        read_property(req, object, name, read)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Option<u32> {
        property_slot(req, object, name)
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
    ) -> Result<Option<PhpArray>, Thrown> {
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
        if let Some(parent) = parent {
            req.release(parent.as_value());
        }
    }
}
