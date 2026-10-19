//! lexbor's C interface as the vendored headers declare it (lexbor 2.7.0,
//! php-src's copy with php's patches applied): the structs ext/dom's C
//! reads laid out as C lays them out, and the functions it calls. Where a
//! header offers a `_noi` function beside an inline one, the `_noi` one is
//! called, so the fewer layouts are mirrored; the rest are mirrored up to
//! the last field read, and the crate's tests hold them.
#![allow(non_camel_case_types, dead_code, missing_docs)]

use std::ffi::{c_long, c_uint, c_void};

pub(crate) type lxb_status_t = c_uint;
pub(crate) type lxb_codepoint_t = u32;

/// `lexbor_status_t`.
pub(crate) const LXB_STATUS_OK: lxb_status_t = 0x0000;
pub(crate) const LXB_STATUS_CONTINUE: lxb_status_t = 0x000e;
pub(crate) const LXB_STATUS_SMALL_BUFFER: lxb_status_t = 0x000f;

/// `lxb_dom_node_type_t`.
pub(crate) const LXB_DOM_NODE_TYPE_ELEMENT: c_uint = 0x01;
pub(crate) const LXB_DOM_NODE_TYPE_ATTRIBUTE: c_uint = 0x02;
pub(crate) const LXB_DOM_NODE_TYPE_TEXT: c_uint = 0x03;
pub(crate) const LXB_DOM_NODE_TYPE_CDATA_SECTION: c_uint = 0x04;
pub(crate) const LXB_DOM_NODE_TYPE_PROCESSING_INSTRUCTION: c_uint = 0x07;
pub(crate) const LXB_DOM_NODE_TYPE_COMMENT: c_uint = 0x08;
pub(crate) const LXB_DOM_NODE_TYPE_DOCUMENT: c_uint = 0x09;
pub(crate) const LXB_DOM_NODE_TYPE_DOCUMENT_TYPE: c_uint = 0x0a;
pub(crate) const LXB_DOM_NODE_TYPE_DOCUMENT_FRAGMENT: c_uint = 0x0b;

/// `lxb_tag_id_t`'s one value read here.
pub(crate) const LXB_TAG_TEMPLATE: usize = 0x00b5;
pub(crate) const LXB_TAG__UNDEF: usize = 0x0000;

/// `lxb_ns_id_t`.
pub(crate) const LXB_NS__UNDEF: usize = 0x00;
pub(crate) const LXB_NS_HTML: usize = 0x02;
pub(crate) const LXB_NS_MATH: usize = 0x03;
pub(crate) const LXB_NS_SVG: usize = 0x04;
pub(crate) const LXB_NS_XLINK: usize = 0x05;
pub(crate) const LXB_NS_XML: usize = 0x06;
pub(crate) const LXB_NS_XMLNS: usize = 0x07;

/// `lxb_encoding_t`'s values named here.
pub(crate) const LXB_ENCODING_UTF_16BE: c_uint = 0x19;
pub(crate) const LXB_ENCODING_UTF_16LE: c_uint = 0x1a;
pub(crate) const LXB_ENCODING_UTF_8: c_uint = 0x1b;

/// `lxb_css_selector_type_t`.
pub(crate) const LXB_CSS_SELECTOR_TYPE_ANY: c_uint = 0x01;
pub(crate) const LXB_CSS_SELECTOR_TYPE_ELEMENT: c_uint = 0x02;
pub(crate) const LXB_CSS_SELECTOR_TYPE_ID: c_uint = 0x03;
pub(crate) const LXB_CSS_SELECTOR_TYPE_CLASS: c_uint = 0x04;
pub(crate) const LXB_CSS_SELECTOR_TYPE_ATTRIBUTE: c_uint = 0x05;
pub(crate) const LXB_CSS_SELECTOR_TYPE_PSEUDO_CLASS: c_uint = 0x06;
pub(crate) const LXB_CSS_SELECTOR_TYPE_PSEUDO_CLASS_FUNCTION: c_uint = 0x07;
pub(crate) const LXB_CSS_SELECTOR_TYPE_PSEUDO_ELEMENT: c_uint = 0x08;
pub(crate) const LXB_CSS_SELECTOR_TYPE_PSEUDO_ELEMENT_FUNCTION: c_uint = 0x09;

/// `lxb_css_selector_pseudo_class_function_id_t`'s values that carry data.
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_CURRENT: c_uint = 0x01;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_HAS: c_uint = 0x03;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_IS: c_uint = 0x04;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NOT: c_uint = 0x07;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_CHILD: c_uint = 0x08;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_COL: c_uint = 0x09;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_LAST_CHILD: c_uint = 0x0a;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_LAST_COL: c_uint = 0x0b;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_LAST_OF_TYPE: c_uint = 0x0c;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_OF_TYPE: c_uint = 0x0d;
pub(crate) const LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_WHERE: c_uint = 0x0e;

/// `lexbor_str_t`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct lexbor_str_t {
    pub data: *mut u8,
    pub length: usize,
}

/// `lexbor_array_obj_t`.
#[repr(C)]
pub(crate) struct lexbor_array_obj_t {
    pub list: *mut c_void,
    pub size: usize,
    pub length: usize,
    pub struct_size: usize,
}

/// `lexbor_hash_entry_t`.
#[repr(C)]
pub(crate) struct lexbor_hash_entry_t {
    pub u: [u8; 17],
    pub length: usize,
    pub next: *mut c_void,
}

/// `lxb_ns_data_t`.
#[repr(C)]
pub(crate) struct lxb_ns_data_t {
    pub entry: lexbor_hash_entry_t,
    pub ns_id: usize,
    pub ref_count: usize,
    pub read_only: bool,
}

/// `lxb_dom_node_t` (`LXB_DOM_NODE_USER_VARIABLES` undefined).
#[repr(C)]
pub(crate) struct lxb_dom_node_t {
    pub events: *mut c_void,
    pub local_name: usize,
    pub prefix: usize,
    pub ns: usize,
    pub owner_document: *mut lxb_dom_document_t,
    pub next: *mut lxb_dom_node_t,
    pub prev: *mut lxb_dom_node_t,
    pub parent: *mut lxb_dom_node_t,
    pub first_child: *mut lxb_dom_node_t,
    pub last_child: *mut lxb_dom_node_t,
    pub user: *mut c_void,
    pub type_: c_uint,
    pub line: usize,
}

/// `lxb_dom_element_t`.
#[repr(C)]
pub(crate) struct lxb_dom_element_t {
    pub node: lxb_dom_node_t,
    pub upper_name: usize,
    pub qualified_name: usize,
    pub is_value: *mut lexbor_str_t,
    pub first_attr: *mut lxb_dom_attr_t,
    pub last_attr: *mut lxb_dom_attr_t,
    pub attr_id: *mut lxb_dom_attr_t,
    pub attr_class: *mut lxb_dom_attr_t,
    pub style: *mut c_void,
    pub list: *mut c_void,
    pub custom_state: c_uint,
}

/// `lxb_dom_attr_t`.
#[repr(C)]
pub(crate) struct lxb_dom_attr_t {
    pub node: lxb_dom_node_t,
    pub upper_name: usize,
    pub qualified_name: usize,
    pub value: *mut lexbor_str_t,
    pub owner: *mut lxb_dom_element_t,
    pub next: *mut lxb_dom_attr_t,
    pub prev: *mut lxb_dom_attr_t,
}

/// `lxb_dom_character_data_t`, the head of text and comment nodes.
#[repr(C)]
pub(crate) struct lxb_dom_character_data_t {
    pub node: lxb_dom_node_t,
    pub data: lexbor_str_t,
}

/// `lxb_html_template_element_t`: the element, then its content
/// fragment, whose head is a node.
#[repr(C)]
pub(crate) struct lxb_html_template_element_t {
    pub element: lxb_dom_element_t,
    pub content: *mut lxb_dom_node_t,
}

/// `lxb_dom_document_t`; `lxb_html_document_t` begins with one.
#[repr(C)]
pub(crate) struct lxb_dom_document_t {
    pub node: lxb_dom_node_t,
    pub compat: c_uint,
    pub type_: c_uint,
    pub doctype: *mut c_void,
    pub element: *mut c_void,
    pub create_interface: *mut c_void,
    pub clone_interface: *mut c_void,
    pub destroy_interface: *mut c_void,
    pub node_cb: *const c_void,
    pub mraw: *mut c_void,
    pub text: *mut c_void,
    pub tags: *mut c_void,
    pub attrs: *mut c_void,
    pub prefix: *mut c_void,
    pub ns: *mut c_void,
    pub parser: *mut c_void,
    pub user: *mut c_void,
    pub css: *mut c_void,
    pub tags_inherited: bool,
    pub ns_inherited: bool,
    pub scripting: bool,
}

/// `lxb_html_tokenizer_t` up to `parse_errors`.
#[repr(C)]
pub(crate) struct lxb_html_tokenizer_head_t {
    pub state: *mut c_void,
    pub state_return: *mut c_void,
    pub callback_token_done: *mut c_void,
    pub callback_token_ctx: *mut c_void,
    pub tags: *mut c_void,
    pub attrs: *mut c_void,
    pub attrs_mraw: *mut c_void,
    pub mraw: *mut c_void,
    pub token: *mut c_void,
    pub dobj_token: *mut c_void,
    pub dobj_token_attr: *mut c_void,
    pub parse_errors: *mut lexbor_array_obj_t,
}

/// `lxb_html_tree_t` up to `has_explicit_body_tag` (php's patch 0002).
#[repr(C)]
pub(crate) struct lxb_html_tree_head_t {
    pub tkz_ref: *mut c_void,
    pub document: *mut c_void,
    pub fragment: *mut c_void,
    pub form: *mut c_void,
    pub open_elements: *mut c_void,
    pub active_formatting: *mut c_void,
    pub template_insertion_modes: *mut c_void,
    pub pending_text_list: *mut c_void,
    pub pending_have_non_ws: bool,
    pub parse_errors: *mut lexbor_array_obj_t,
    pub foster_parenting: bool,
    pub frameset_ok: bool,
    pub scripting: bool,
    pub has_explicit_html_tag: bool,
    pub has_explicit_head_tag: bool,
    pub has_explicit_body_tag: bool,
}

/// `lxb_html_tokenizer_error_t`.
#[repr(C)]
pub(crate) struct lxb_html_tokenizer_error_t {
    pub pos: *const u8,
    pub id: c_uint,
}

/// `lxb_html_tree_error_t` (php's patch 0001 adds the position).
#[repr(C)]
pub(crate) struct lxb_html_tree_error_t {
    pub id: c_uint,
    pub line: usize,
    pub column: usize,
    pub length: usize,
}

pub(crate) type lxb_encoding_encode_f = unsafe extern "C" fn(
    ctx: *mut lxb_encoding_encode_t,
    cp: *mut *const lxb_codepoint_t,
    end: *const lxb_codepoint_t,
) -> lxb_status_t;

pub(crate) type lxb_encoding_decode_f = unsafe extern "C" fn(
    ctx: *mut lxb_encoding_decode_t,
    data: *mut *const u8,
    end: *const u8,
) -> lxb_status_t;

/// `lxb_encoding_data_t`.
#[repr(C)]
pub(crate) struct lxb_encoding_data_t {
    pub encoding: c_uint,
    pub encode: lxb_encoding_encode_f,
    pub decode: lxb_encoding_decode_f,
    pub encode_single: *const c_void,
    pub decode_single: *const c_void,
    pub name: *const u8,
}

/// `lxb_encoding_decode_t`; the per-encoding state union is 16 bytes, its
/// first word `u.utf_8.need`.
#[repr(C)]
pub(crate) struct lxb_encoding_decode_t {
    pub encoding_data: *const lxb_encoding_data_t,
    pub buffer_out: *mut lxb_codepoint_t,
    pub buffer_length: usize,
    pub buffer_used: usize,
    pub replace_to: *const lxb_codepoint_t,
    pub replace_len: usize,
    pub codepoint: lxb_codepoint_t,
    pub second_codepoint: lxb_codepoint_t,
    pub prepend: bool,
    pub have_error: bool,
    pub status: lxb_status_t,
    pub u: [c_uint; 4],
}

/// `lxb_encoding_encode_t`.
#[repr(C)]
pub(crate) struct lxb_encoding_encode_t {
    pub encoding_data: *const lxb_encoding_data_t,
    pub buffer_out: *mut u8,
    pub buffer_length: usize,
    pub buffer_used: usize,
    pub replace_to: *const u8,
    pub replace_len: usize,
    pub state: c_uint,
}

/// `lxb_html_encoding_t`.
#[repr(C)]
pub(crate) struct lxb_html_encoding_t {
    pub cache: lexbor_array_obj_t,
    pub found: lexbor_array_obj_t,
}

/// `lxb_html_encoding_entry_t`.
#[repr(C)]
pub(crate) struct lxb_html_encoding_entry_t {
    pub name: *const u8,
    pub end: *const u8,
}

/// `lxb_css_selector_attribute_t`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct lxb_css_selector_attribute_t {
    pub match_: c_uint,
    pub modifier: c_uint,
    pub value: lexbor_str_raw_t,
}

/// `lexbor_str_t` where it sits in a union and must be `Copy`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct lexbor_str_raw_t {
    pub data: *mut u8,
    pub length: usize,
}

/// `lxb_css_selector_pseudo_t`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct lxb_css_selector_pseudo_t {
    pub type_: c_uint,
    pub data: *mut c_void,
}

/// `union lxb_css_selector_u`.
#[repr(C)]
pub(crate) union lxb_css_selector_u {
    pub attribute: lxb_css_selector_attribute_t,
    pub pseudo: lxb_css_selector_pseudo_t,
}

/// `lxb_css_selector_t`.
#[repr(C)]
pub(crate) struct lxb_css_selector_t {
    pub type_: c_uint,
    pub combinator: c_uint,
    pub name: lexbor_str_t,
    pub ns: lexbor_str_t,
    pub u: lxb_css_selector_u,
    pub next: *mut lxb_css_selector_t,
    pub prev: *mut lxb_css_selector_t,
    pub list: *mut lxb_css_selector_list_t,
}

/// `lxb_css_selector_list_t`.
#[repr(C)]
pub(crate) struct lxb_css_selector_list_t {
    pub first: *mut lxb_css_selector_t,
    pub last: *mut lxb_css_selector_t,
    pub parent: *mut lxb_css_selector_t,
    pub next: *mut lxb_css_selector_list_t,
    pub prev: *mut lxb_css_selector_list_t,
    pub memory: *mut c_void,
    pub specificity: u32,
}

/// `lxb_css_selector_anb_of_t`.
#[repr(C)]
pub(crate) struct lxb_css_selector_anb_of_t {
    pub a: c_long,
    pub b: c_long,
    pub of: *mut lxb_css_selector_list_t,
}

/// `lxb_css_log_message_t`.
#[repr(C)]
pub(crate) struct lxb_css_log_message_t {
    pub text: lexbor_str_t,
    pub type_: c_uint,
}

/// `lxb_css_log_t` up to its messages.
#[repr(C)]
pub(crate) struct lxb_css_log_t {
    pub messages: lexbor_array_obj_t,
}

/// `lxb_url_host_type_t`.
pub(crate) const LXB_URL_HOST_TYPE_DOMAIN: c_uint = 0x01;
pub(crate) const LXB_URL_HOST_TYPE_OPAQUE: c_uint = 0x02;
pub(crate) const LXB_URL_HOST_TYPE_IPV4: c_uint = 0x03;
pub(crate) const LXB_URL_HOST_TYPE_IPV6: c_uint = 0x04;
pub(crate) const LXB_URL_HOST_TYPE_EMPTY: c_uint = 0x05;

/// `lexbor_serialize_cb_f`.
pub(crate) type lexbor_serialize_cb_f =
    unsafe extern "C" fn(data: *const u8, len: usize, ctx: *mut c_void) -> lxb_status_t;

/// `lxb_url_scheme_t`.
#[repr(C)]
pub(crate) struct lxb_url_scheme_t {
    pub(crate) name: lexbor_str_t,
    pub(crate) type_: c_uint,
}

/// The union of `lxb_url_host_t`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union lxb_url_host_u {
    pub(crate) ipv6: [u16; 8],
    pub(crate) ipv4: u32,
    pub(crate) opaque: lexbor_str_t,
    pub(crate) domain: lexbor_str_t,
}

/// `lxb_url_host_t`.
#[repr(C)]
pub(crate) struct lxb_url_host_t {
    pub(crate) type_: c_uint,
    pub(crate) u: lxb_url_host_u,
}

/// `lxb_url_path_t`.
#[repr(C)]
pub(crate) struct lxb_url_path_t {
    pub(crate) str_: lexbor_str_t,
    pub(crate) length: usize,
    pub(crate) opaque: bool,
}

/// `lxb_url_t`.
#[repr(C)]
pub(crate) struct lxb_url_t {
    pub(crate) scheme: lxb_url_scheme_t,
    pub(crate) host: lxb_url_host_t,
    pub(crate) username: lexbor_str_t,
    pub(crate) password: lexbor_str_t,
    pub(crate) port: u16,
    pub(crate) has_port: bool,
    pub(crate) path: lxb_url_path_t,
    pub(crate) query: lexbor_str_t,
    pub(crate) fragment: lexbor_str_t,
    pub(crate) mraw: *mut c_void,
}

/// `lexbor_plog_entry_t`.
#[repr(C)]
pub(crate) struct lexbor_plog_entry_t {
    pub(crate) data: *const u8,
    pub(crate) context: *mut c_void,
    pub(crate) id: c_uint,
}

/// `lxb_url_parser_t`: `log` is a `lexbor_plog_t`, whose one field is the
/// `lexbor_array_obj_t` of its entries.
#[repr(C)]
pub(crate) struct lxb_url_parser_t {
    pub(crate) url: *mut lxb_url_t,
    pub(crate) mraw: *mut lexbor_mraw_t,
    pub(crate) log: *mut lexbor_array_obj_t,
    pub(crate) idna: *mut c_void,
    pub(crate) buffer: *mut u8,
}

/// `lexbor_mraw_t`: the pool's memory, its cache of freed blocks and its
/// count, each null until `lexbor_mraw_init`.
#[repr(C)]
pub(crate) struct lexbor_mraw_t {
    pub(crate) mem: *mut c_void,
    pub(crate) cache: *mut c_void,
    pub(crate) ref_count: usize,
}

/// `lxb_unicode_idna_t`, whose one field is a `lxb_unicode_normalizer_t`
/// (`unicode/base.h`), every field null or zero until
/// `lxb_unicode_idna_init`.
#[repr(C)]
pub(crate) struct lxb_unicode_idna_t {
    pub(crate) decomposition: *mut c_void,
    pub(crate) composition: *mut c_void,
    pub(crate) starter: *mut c_void,
    pub(crate) buf: *mut c_void,
    pub(crate) end: *const c_void,
    pub(crate) p: *mut c_void,
    pub(crate) ican: *mut c_void,
    pub(crate) tmp: [u8; 4],
    pub(crate) tmp_lenght: u8,
    pub(crate) quick_ccc: u8,
    pub(crate) quick_type: u8,
    pub(crate) flush_cp: usize,
}

/// `lexbor_memory_malloc_f`, `lexbor_memory_realloc_f`,
/// `lexbor_memory_calloc_f` and `lexbor_memory_free_f`.
pub(crate) type lexbor_memory_malloc_f = extern "C" fn(size: usize) -> *mut c_void;
pub(crate) type lexbor_memory_realloc_f =
    unsafe extern "C" fn(dst: *mut c_void, size: usize) -> *mut c_void;
pub(crate) type lexbor_memory_calloc_f = extern "C" fn(num: usize, size: usize) -> *mut c_void;
pub(crate) type lexbor_memory_free_f = unsafe extern "C" fn(dst: *mut c_void);

unsafe extern "C" {
    // core/lexbor.h
    pub(crate) fn lexbor_memory_setup(
        new_malloc: lexbor_memory_malloc_f,
        new_realloc: lexbor_memory_realloc_f,
        new_calloc: lexbor_memory_calloc_f,
        new_free: lexbor_memory_free_f,
    ) -> lxb_status_t;

    // core/mraw.h, core/str.h
    pub(crate) fn lexbor_mraw_init(mraw: *mut lexbor_mraw_t, chunk_size: usize) -> lxb_status_t;
    pub(crate) fn lexbor_mraw_destroy(
        mraw: *mut lexbor_mraw_t,
        destroy_self: bool,
    ) -> *mut lexbor_mraw_t;
    pub(crate) fn lexbor_str_init_append(
        str_: *mut lexbor_str_t,
        mraw: *mut lexbor_mraw_t,
        data: *const u8,
        length: usize,
    ) -> *mut u8;

    // unicode/idna.h
    pub(crate) fn lxb_unicode_idna_init(idna: *mut lxb_unicode_idna_t) -> lxb_status_t;
    pub(crate) fn lxb_unicode_idna_clean(idna: *mut lxb_unicode_idna_t);
    pub(crate) fn lxb_unicode_idna_destroy(
        idna: *mut lxb_unicode_idna_t,
        self_destroy: bool,
    ) -> *mut lxb_unicode_idna_t;

    // url/url.h
    pub(crate) fn lxb_url_parser_init(
        parser: *mut lxb_url_parser_t,
        mraw: *mut lexbor_mraw_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_parser_clean(parser: *mut lxb_url_parser_t);
    pub(crate) fn lxb_url_parser_destroy(
        parser: *mut lxb_url_parser_t,
        destroy_self: bool,
    ) -> *mut lxb_url_parser_t;
    pub(crate) fn lxb_url_destroy(url: *mut lxb_url_t) -> *mut lxb_url_t;
    pub(crate) fn lxb_url_parse(
        parser: *mut lxb_url_parser_t,
        base_url: *const lxb_url_t,
        data: *const u8,
        length: usize,
    ) -> *mut lxb_url_t;
    pub(crate) fn lxb_url_clone(mraw: *mut lexbor_mraw_t, url: *const lxb_url_t) -> *mut lxb_url_t;
    pub(crate) fn lxb_url_api_protocol_set(
        url: *mut lxb_url_t,
        parser: *mut lxb_url_parser_t,
        protocol: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_api_username_set(
        url: *mut lxb_url_t,
        username: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_api_password_set(
        url: *mut lxb_url_t,
        password: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_api_hostname_set(
        url: *mut lxb_url_t,
        parser: *mut lxb_url_parser_t,
        hostname: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_api_port_set(
        url: *mut lxb_url_t,
        parser: *mut lxb_url_parser_t,
        port: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_api_pathname_set(
        url: *mut lxb_url_t,
        parser: *mut lxb_url_parser_t,
        pathname: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_api_search_set(
        url: *mut lxb_url_t,
        parser: *mut lxb_url_parser_t,
        search: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_api_hash_set(
        url: *mut lxb_url_t,
        parser: *mut lxb_url_parser_t,
        hash: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_serialize(
        url: *const lxb_url_t,
        cb: lexbor_serialize_cb_f,
        ctx: *mut c_void,
        exclude_fragment: bool,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_serialize_idna(
        idna: *mut lxb_unicode_idna_t,
        url: *const lxb_url_t,
        cb: lexbor_serialize_cb_f,
        ctx: *mut c_void,
        exclude_fragment: bool,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_serialize_host_unicode(
        idna: *mut lxb_unicode_idna_t,
        host: *const lxb_url_host_t,
        cb: lexbor_serialize_cb_f,
        ctx: *mut c_void,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_serialize_host_ipv4(
        ipv4: u32,
        cb: lexbor_serialize_cb_f,
        ctx: *mut c_void,
    ) -> lxb_status_t;
    pub(crate) fn lxb_url_serialize_host_ipv6(
        ipv6: *const u16,
        cb: lexbor_serialize_cb_f,
        ctx: *mut c_void,
    ) -> lxb_status_t;
}

unsafe extern "C" {
    // html/interfaces/document.h
    pub(crate) fn lxb_html_document_create() -> *mut lxb_dom_document_t;
    pub(crate) fn lxb_html_document_destroy(document: *mut lxb_dom_document_t) -> *mut c_void;
    pub(crate) fn lxb_html_document_parse_chunk_begin(
        document: *mut lxb_dom_document_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_html_document_parse_chunk(
        document: *mut lxb_dom_document_t,
        html: *const u8,
        size: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_html_document_parse_chunk_end(
        document: *mut lxb_dom_document_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_html_document_parse_fragment_chunk_begin(
        document: *mut lxb_dom_document_t,
        element: *mut lxb_dom_element_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_html_document_parse_fragment_chunk(
        document: *mut lxb_dom_document_t,
        html: *const u8,
        size: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_html_document_parse_fragment_chunk_end(
        document: *mut lxb_dom_document_t,
    ) -> *mut lxb_dom_node_t;

    // html/parser.h
    pub(crate) fn lxb_html_parser_tokenizer_noi(parser: *mut c_void) -> *mut c_void;
    pub(crate) fn lxb_html_parser_tree_noi(parser: *mut c_void) -> *mut c_void;

    // dom/interfaces/element.h, attr.h, document_type.h
    pub(crate) fn lxb_dom_element_interface_create(
        document: *mut lxb_dom_document_t,
    ) -> *mut lxb_dom_element_t;
    pub(crate) fn lxb_dom_element_qualified_name(
        element: *mut lxb_dom_element_t,
        len: *mut usize,
    ) -> *const u8;
    pub(crate) fn lxb_dom_attr_qualified_name(
        attr: *mut lxb_dom_attr_t,
        len: *mut usize,
    ) -> *const u8;
    pub(crate) fn lxb_dom_attr_value_noi(attr: *mut lxb_dom_attr_t, len: *mut usize) -> *const u8;
    pub(crate) fn lxb_dom_document_type_name_noi(
        doc_type: *mut lxb_dom_node_t,
        len: *mut usize,
    ) -> *const u8;
    pub(crate) fn lxb_dom_document_type_public_id_noi(
        doc_type: *mut lxb_dom_node_t,
        len: *mut usize,
    ) -> *const u8;
    pub(crate) fn lxb_dom_document_type_system_id_noi(
        doc_type: *mut lxb_dom_node_t,
        len: *mut usize,
    ) -> *const u8;

    // tag/tag.h, ns/ns.h
    pub(crate) fn lxb_tag_id_by_name_noi(hash: *mut c_void, name: *const u8, len: usize) -> usize;
    pub(crate) fn lxb_ns_data_by_link(
        hash: *mut c_void,
        name: *const u8,
        length: usize,
    ) -> *const lxb_ns_data_t;

    // encoding/encoding.h, decode.h, encode.h
    pub(crate) fn lxb_encoding_data_by_name(
        name: *const u8,
        length: usize,
    ) -> *const lxb_encoding_data_t;
    pub(crate) fn lxb_encoding_data_by_pre_name(
        name: *const u8,
        length: usize,
    ) -> *const lxb_encoding_data_t;
    pub(crate) fn lxb_encoding_data_noi(encoding: c_uint) -> *const lxb_encoding_data_t;
    pub(crate) fn lxb_encoding_decode_init_noi(
        decode: *mut lxb_encoding_decode_t,
        encoding_data: *const lxb_encoding_data_t,
        buffer_out: *mut lxb_codepoint_t,
        buffer_length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_decode_init_single_noi(
        decode: *mut lxb_encoding_decode_t,
        encoding_data: *const lxb_encoding_data_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_decode_replace_set_noi(
        decode: *mut lxb_encoding_decode_t,
        replace: *const lxb_codepoint_t,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_decode_buf_used_noi(decode: *mut lxb_encoding_decode_t) -> usize;
    pub(crate) fn lxb_encoding_decode_buf_used_set_noi(
        decode: *mut lxb_encoding_decode_t,
        buffer_used: usize,
    );
    pub(crate) fn lxb_encoding_decode_finish_noi(
        decode: *mut lxb_encoding_decode_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_decode_utf_8_single(
        ctx: *mut lxb_encoding_decode_t,
        data: *mut *const u8,
        end: *const u8,
    ) -> lxb_codepoint_t;
    pub(crate) fn lxb_encoding_encode_init_noi(
        encode: *mut lxb_encoding_encode_t,
        encoding_data: *const lxb_encoding_data_t,
        buffer_out: *mut u8,
        buffer_length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_encode_init_single_noi(
        encode: *mut lxb_encoding_encode_t,
        encoding_data: *const lxb_encoding_data_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_encode_replace_set_noi(
        encode: *mut lxb_encoding_encode_t,
        replace: *const u8,
        length: usize,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_encode_buf_used_noi(encode: *mut lxb_encoding_encode_t) -> usize;
    pub(crate) fn lxb_encoding_encode_buf_used_set_noi(
        encode: *mut lxb_encoding_encode_t,
        buffer_used: usize,
    );
    pub(crate) fn lxb_encoding_encode_finish_noi(
        encode: *mut lxb_encoding_encode_t,
    ) -> lxb_status_t;
    pub(crate) fn lxb_encoding_encode_utf_8_single(
        ctx: *mut lxb_encoding_encode_t,
        data: *mut *mut u8,
        end: *const u8,
        cp: lxb_codepoint_t,
    ) -> i8;

    // html/encoding.h
    pub(crate) fn lxb_html_encoding_init(em: *mut lxb_html_encoding_t) -> lxb_status_t;
    pub(crate) fn lxb_html_encoding_destroy(
        em: *mut lxb_html_encoding_t,
        self_destroy: bool,
    ) -> *mut lxb_html_encoding_t;
    pub(crate) fn lxb_html_encoding_determine(
        em: *mut lxb_html_encoding_t,
        data: *const u8,
        end: *const u8,
    ) -> lxb_status_t;

    // css/parser.h, css/selectors/selectors.h, selector.h
    pub(crate) fn lxb_css_parser_create() -> *mut c_void;
    pub(crate) fn lxb_css_parser_init(parser: *mut c_void, tkz: *mut c_void) -> lxb_status_t;
    pub(crate) fn lxb_css_parser_destroy(parser: *mut c_void, self_destroy: bool) -> *mut c_void;
    pub(crate) fn lxb_css_parser_log_noi(parser: *mut c_void) -> *mut lxb_css_log_t;
    pub(crate) fn lxb_css_selectors_parse(
        parser: *mut c_void,
        data: *const u8,
        length: usize,
    ) -> *mut lxb_css_selector_list_t;
    pub(crate) fn lxb_css_selector_list_destroy_memory(list: *mut lxb_css_selector_list_t);
}
