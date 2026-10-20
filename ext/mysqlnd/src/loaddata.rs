//! `mysqlnd_loaddata.c`: `LOAD DATA LOCAL INFILE`, the file the server
//! asked for sent in packets of up to 4096 bytes, under php's checks:
//! refused unless `CLIENT_LOCAL_FILES` is on or a local infile directory
//! is set, the directory opened to prove it is there, a file outside it
//! refused where only the directory allows, `open_basedir` held, and the
//! server's answer read whatever happened.

use php_ext::raising::Thrown;
use php_ext::request::{Opening, OwnedStream as _, Request, Streams as _};

use crate::conn::ConnData;
use crate::consts::{Com, client, cr};

/// `lost_conn`.
const LOST_CONN: &str = "Lost connection to MySQL server during LOAD DATA of a local file";

/// `buflen` less the frame header.
const CHUNK: usize = 4096 - crate::net::HEADER_SIZE;

/// `mysqlnd_handle_local_infile`: whether it passed, and php's
/// `is_warning`, which leaves the connection usable after a refusal.
///
/// ```php
/// $file = tempnam(sys_get_temp_dir(), 'li');
/// file_put_contents($file, "1,a\n2,b\n");
/// $root = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// $root->exec('SET GLOBAL local_infile = 1');
/// $li = 'li_' . bin2hex(random_bytes(4));
/// $root->exec("CREATE TABLE $li (id INT, l CHAR(1))");
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [Pdo\Mysql::ATTR_LOCAL_INFILE => true]);
/// var_dump($db->exec("LOAD DATA LOCAL INFILE '$file' INTO TABLE $li FIELDS TERMINATED BY ','"));
/// var_dump($db->query("SELECT COUNT(*), GROUP_CONCAT(l) FROM $li")->fetch(PDO::FETCH_NUM));
/// $no = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// try { $no->exec("LOAD DATA LOCAL INFILE '$file' INTO TABLE $li"); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
/// $root->exec("DROP TABLE $li");
/// $root->exec('SET GLOBAL local_infile = 0');
/// unlink($file);
/// ```
/// ```expect mysql
/// int(2)
/// array(2) {
///   [0]=>
///   int(2)
///   [1]=>
///   string(3) "a,b"
/// }
/// SQLSTATE[HY000]: General error: 2068 LOAD DATA LOCAL INFILE is forbidden, check related settings like mysqli.allow_local_infile|mysqli.local_infile_directory or Pdo\Mysql::ATTR_LOCAL_INFILE|Pdo\Mysql::ATTR_LOCAL_INFILE_DIRECTORY
/// ```
pub(crate) fn handle(
    req: &mut Request,
    conn: &mut ConnData,
    filename: &[u8],
) -> Result<(bool, bool), Thrown> {
    let enabled = conn.options.flags & client::LOCAL_FILES != 0;
    let directory = conn
        .options
        .local_infile_directory
        .as_ref()
        .map(|d| d.bytes().to_vec());
    let mut ok = true;
    if !enabled && directory.is_none() {
        conn.error.client(
            req,
            cr::LOAD_DATA_LOCAL_INFILE_REJECTED,
            "LOAD DATA LOCAL INFILE is forbidden, check related settings like mysqli.allow_local_infile|mysqli.local_infile_directory or Pdo\\Mysql::ATTR_LOCAL_INFILE|Pdo\\Mysql::ATTR_LOCAL_INFILE_DIRECTORY",
        )?;
        ok = false;
    }
    if let Some(dir) = &directory {
        if let Some(stream) = req.open_dir(dir)? {
            stream.free(req);
        } else {
            conn.error.client(
                req,
                cr::LOAD_DATA_LOCAL_INFILE_REJECTED,
                "cannot open local_infile_directory",
            )?;
            ok = false;
        }
    }
    if ok
        && !enabled
        && let Some(dir) = &directory
        && !req.environment_mut().within_basedir(dir, filename)
    {
        conn.error.client(
            req,
            cr::LOAD_DATA_LOCAL_INFILE_REJECTED,
            "LOAD DATA LOCAL INFILE DIRECTORY restriction in effect. Unable to open file",
        )?;
        ok = false;
    }
    if !ok {
        conn.net.send(req, &[], &mut conn.stats, &mut conn.error)?;
        conn.handle_response(req, false, false, Com::Query, false)?;
        return Ok((false, true));
    }
    let (passed, warning) = send_file(req, conn, filename)?;
    let answered = conn.handle_response(req, false, false, Com::Query, false)?;
    Ok((passed && answered, warning))
}

/// The init, read and error handlers php sets with
/// `mysqlnd_local_infile_default`, and the loop that sends what they
/// read: `Ok((passed, is_warning))`.
fn send_file(
    req: &mut Request,
    conn: &mut ConnData,
    filename: &[u8],
) -> Result<(bool, bool), Thrown> {
    // `mysqlnd_local_infile_init`.
    let open_basedir = req
        .ini()
        .get(b"open_basedir")
        .is_some_and(|v| !v.is_empty());
    let refused = |conn: &mut ConnData, req: &mut Request, errno: u32, text: &str| {
        conn.error.client(req, errno, text)?;
        conn.net.send(req, &[], &mut conn.stats, &mut conn.error)?;
        Ok::<_, Thrown>((false, true))
    };
    if open_basedir
        && req
            .environment_mut()
            .check_open_basedir(filename, false)
            .is_err()
    {
        return refused(
            conn,
            req,
            cr::UNKNOWN_ERROR,
            "open_basedir restriction in effect. Unable to open file",
        );
    }
    let opening = Opening {
        use_path: false,
        warn: false,
        for_include: false,
        must_seek: false,
        will_cast: false,
        disable_open_basedir: false,
        only_get_headers: false,
        ..Opening::default()
    };
    let Some(stream) = req.open_stream_with(filename, b"r", opening)? else {
        let shown: String = String::from_utf8_lossy(filename).chars().take(64).collect();
        return refused(
            conn,
            req,
            cr::EE_FILENOTFOUND,
            &format!("Can't find file '{shown}'."),
        );
    };
    let mut failed_read = false;
    // `mysqlnd_local_infile_read`'s `php_stream_read` into the buffer
    // the loop sends from, with no string of the request's between.
    let mut buf = [0; CHUNK];
    loop {
        let chunk = match stream.read_into(req, &mut buf) {
            Some(0) => break,
            Some(read) => &buf[..read],
            None => {
                failed_read = true;
                break;
            }
        };
        if conn
            .net
            .send(req, chunk, &mut conn.stats, &mut conn.error)?
            == 0
        {
            conn.error.client(req, cr::SERVER_LOST, LOST_CONN)?;
            stream.free(req);
            return Ok((false, false));
        }
    }
    stream.free(req);
    if conn.net.send(req, &[], &mut conn.stats, &mut conn.error)? == 0 {
        conn.error.client(req, cr::SERVER_LOST, LOST_CONN)?;
        return Ok((false, false));
    }
    if failed_read {
        conn.error
            .client(req, cr::UNKNOWN_ERROR, "Error reading file")?;
        return Ok((false, true));
    }
    Ok((true, false))
}
