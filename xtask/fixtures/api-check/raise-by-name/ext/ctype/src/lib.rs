fn ctype_alpha(req: &mut Request) -> Thrown {
    throw(req, lookup_class(req, b"ValueError")?, "a raise by its class's name")
}
