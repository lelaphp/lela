fn json_decode(req: &mut Request, ce: &Classes) -> Thrown {
    throw(req, ce.json_exception, "a raise by its handle")
}
