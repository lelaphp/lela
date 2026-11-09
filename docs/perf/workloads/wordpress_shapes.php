<?php
// wordpress_shapes: original code whose opline mix follows what #4329 counted over four WordPress bench pages; none of its
// code comes from WordPress, and only the side panels' sizes and sort orders follow its widget defaults. It renders a
// small site three ways (the newest entries, one entry, a search) many times over: a lexer
// and a table-driven LL(1) parser for a small query language, an event registry whose listeners are callables kept in arrays
// and run through `call_user_func_array`, a grouped cache, a router reading request arrays behind guard clauses, and records
// with nullable declared properties read through `??` and through chains of `->`. The functions after the main code are
// called before their declaration, so those calls resolve by name. Takes no arguments; prints counts and crc32 checksums.

// Each nonterminal maps to its alternatives, an empty one matching nothing; the upper-case words are token kinds.
const SYNTAX = [
    'query' => ['FIND WORD filter order window END'],
    'filter' => ['WHERE either', ''], 'either' => ['both either_more'], 'either_more' => ['OR both either_more', ''],
    'both' => ['test both_more'], 'both_more' => ['AND test both_more', ''],
    'test' => ['NOT test', 'OPEN either CLOSE', 'WORD check'],
    'check' => ['IS value', 'IN OPEN values CLOSE', 'HAS value', 'ABOVE NUMBER', 'BELOW NUMBER'],
    'values' => ['value values_more'], 'values_more' => ['COMMA value values_more', ''], 'value' => ['WORD', 'TEXT', 'NUMBER'],
    'order' => ['SORT WORD direction order_more', ''], 'order_more' => ['COMMA WORD direction order_more', ''],
    'direction' => ['UP', 'DOWN', ''], 'window' => ['TAKE NUMBER skip', ''], 'skip' => ['SKIP NUMBER', ''],
];
const STARTUP = ['process_start', 'config_read', 'modules_ready', 'skin_ready', 'routes_built', 'session_open', 'locale_set',
    'assets_queued', 'head_open', 'head_close', 'body_open', 'body_close', 'response_sent', 'process_end'];
const SETTINGS = ['base_url', 'name', 'motto', 'encoding', 'zone', 'day_format', 'clock_format', 'page_size', 'url_style', 'skin',
    'modules', 'start_page', 'pinned', 'locale', 'media_dir', 'schedule', 'replies_open', 'feed_size'];
// Each side panel: its heading, the request variables it lists by, and the query it runs.
const PANELS = [
    ['Latest', ['kind' => 'post', 'size' => 5], 'find entries where kind is post and state is live sort stamp down take 5'],
    ['Pages', ['kind' => 'page'], 'find entries where kind is page and state is live sort position up, title up'],
    ['Months', ['year' => 2023], 'find entries where kind is post and stamp above 1672531200 sort stamp down'],
    ['Links', ['kind' => 'page', 'size' => 3], 'find links where (kind is link or kind is page) and not state is hidden sort position up'],
    ['People', ['by' => 1, 'size' => 3], 'find people sort name up take 20'],
    ['Talked about', ['size' => 4], 'find entries where state is live and notes above 0 sort notes down take 4'],
    ['Labels', ['label' => 'engine'], "find labels where uses above 0 and kind in ('topic', 'series', 'format') sort uses down take 45"],
];
// The attributes each element may carry: true for any value, a string for a value that must start with it.
const ATTRIBUTES = ['article' => ['id' => true, 'class' => true, 'data-pinned' => true, 'data-notes' => true],
    'a' => ['href' => '/', 'class' => true, 'rel' => true], 'time' => ['datetime' => true],
    'link' => ['rel' => true, 'href' => 'https://', 'media' => true, 'id' => true]];

final class Token { public function __construct(public int $kind, public string $text) {} }

final class Node { public array $parts = []; public function __construct(public string $name) {} }

final class Lexer {
    public const KEYWORDS = ['FIND' => 1, 'WHERE' => 2, 'OR' => 3, 'AND' => 4, 'NOT' => 5, 'IS' => 6, 'IN' => 7, 'HAS' => 8,
        'ABOVE' => 9, 'BELOW' => 10, 'SORT' => 11, 'UP' => 12, 'DOWN' => 13, 'TAKE' => 14, 'SKIP' => 15];
    public const WORD = 16, TEXT = 17, NUMBER = 18, COMMA = 19, OPEN = 20, CLOSE = 21, END = 22;
    public const KINDS = self::KEYWORDS + ['WORD' => 16, 'TEXT' => 17, 'NUMBER' => 18, 'COMMA' => 19, 'OPEN' => 20, 'CLOSE' => 21, 'END' => 22];
    private const PUNCTUATION = [',' => self::COMMA, '(' => self::OPEN, ')' => self::CLOSE];
    private const LETTERS = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789';

    private int $at = 0, $length;
    public function __construct(private string $source) { $this->length = strlen($source); }

    // The query's tokens, ending with an END token.
    public function tokens() {
        $tokens = [];
        while ($this->at < $this->length) {
            $token = $this->next();
            if (null !== $token) { $tokens[] = $token; }
        }
        $tokens[] = new Token(self::END, '');
        return $tokens;
    }

    // The token at the cursor, or null for a run of spaces or a byte that starts no token; the cursor moves past it either way.
    private function next() {
        $from = $this->at;
        $char = $this->source[$from];
        if (' ' === $char || "\n" === $char) {
            $this->at += strspn($this->source, " \n", $from);
            return null;
        }
        if (isset(self::PUNCTUATION[$char])) {
            $this->at++;
            return new Token(self::PUNCTUATION[$char], $char);
        }
        if ("'" === $char) {
            $close = strpos($this->source, "'", $from + 1);
            $this->at = false === $close ? $this->length : $close + 1;
            return new Token(self::TEXT, substr($this->source, $from + 1, $this->at - $from - 2));
        }
        $span = strspn($this->source, self::LETTERS, $from);
        if (0 === $span) {
            $this->at++;
            return null;
        }
        $this->at += $span;
        $word = substr($this->source, $from, $span);
        $upper = strtoupper($word);
        $kind = self::WORD;
        if ($char >= '0' && $char <= '9') {
            $kind = self::NUMBER;
        } elseif (isset(self::KEYWORDS[$upper])) {
            $kind = self::KEYWORDS[$upper];
        }
        return new Token($kind, $word);
    }
}

// The grammar as numbered rules and a choice table: for each nonterminal and next token kind, the alternative to expand,
// and for a nonterminal with an empty-matching alternative, that alternative when no other fits.
final class Syntax {
    public array $kinds, $rules = [], $names = [], $table = [], $fallback = [];
    public int $terminals;

    public function __construct(array $kinds, array $grammar) {
        $this->kinds = $kinds;
        $this->terminals = $number = count($kinds);
        foreach (array_keys($grammar) as $name) { $this->kinds[$name] = ++$number; $this->names[$number] = $name; }
        foreach ($grammar as $name => $alternatives) {
            foreach ($alternatives as $text) {
                $words = preg_split('/[ ]+/', $text, -1, PREG_SPLIT_NO_EMPTY);
                $this->rules[$this->kinds[$name]][] = array_map(fn($word) => $this->kinds[$word], $words);
            }
        }
        $starts = $this->starts();
        foreach ($this->rules as $rule => $alternatives) {
            foreach ($alternatives as $index => $symbols) {
                [$set, $empty] = $this->startOf($symbols, $starts);
                foreach (array_keys($set) as $kind) { $this->table[$rule][$kind] = $index; }
                if ($empty) { $this->fallback[$rule] = $index; }
            }
        }
    }

    // The token kinds each nonterminal can begin with, and whether it can match nothing, grown until neither changes.
    private function starts() {
        $starts = array_fill_keys(array_keys($this->rules), [[], false]);
        do {
            $grew = false;
            foreach ($this->rules as $rule => $alternatives) {
                foreach ($alternatives as $symbols) {
                    [$set, $empty] = $this->startOf($symbols, $starts);
                    $merged = $starts[$rule][0] + $set;
                    if (count($merged) === count($starts[$rule][0]) && ($starts[$rule][1] || !$empty)) { continue; }
                    $starts[$rule] = [$merged, $starts[$rule][1] || $empty];
                    $grew = true;
                }
            }
        } while ($grew);
        return $starts;
    }

    private function startOf(array $symbols, array $starts) {
        $set = [];
        foreach ($symbols as $symbol) {
            if ($symbol <= $this->terminals) { return [$set + [$symbol => true], false]; }
            $set += $starts[$symbol][0];
            if (!$starts[$symbol][1]) { return [$set, false]; }
        }
        return [$set, true];
    }
}

final class Parser {
    private int $at = 0, $kind = Lexer::END, $terminals;
    private array $table, $fallback, $rules, $names;
    public int $built = 0, $tests = 0;

    public function __construct(Syntax $syntax, private array $tokens) {
        [$this->table, $this->fallback, $this->rules, $this->names] = [$syntax->table, $syntax->fallback, $syntax->rules, $syntax->names];
        $this->terminals = $syntax->terminals;
    }

    public function tree(int $start) {
        $this->at = 0;
        $this->kind = $this->tokens[0]->kind;
        return $this->expand($start);
    }

    // Expands one nonterminal by the alternative the next token selects; null when no alternative fits the input.
    private function expand(int $rule) {
        $choice = null;
        if (isset($this->table[$rule][$this->kind])) {
            $choice = $this->table[$rule][$this->kind];
        } elseif (isset($this->fallback[$rule])) {
            $choice = $this->fallback[$rule];
        }
        if (null === $choice) { return null; }
        $name = $this->names[$rule];
        $node = new Node($name);
        $alternatives = $this->rules[$rule];
        foreach ($alternatives[$choice] as $symbol) {
            if ($symbol <= $this->terminals) {
                if ($this->kind !== $symbol) { return null; }
                $node->parts[] = $this->tokens[$this->at];
                $this->kind = $this->tokens[++$this->at]->kind ?? Lexer::END;
                continue;
            }
            $child = $this->expand($symbol);
            if (null === $child) { return null; }
            if ([] !== $child->parts) { $node->parts[] = $child; }
        }
        $this->built++;
        if ('test' === $name) { $this->tests++; }
        return $node;
    }
}

// Listeners per event name, each [callable, rank, arity], run in rank order and, within a rank, in the order they came.
final class Events {
    public array $listeners = [], $fired = [];
    private array $queues = [];

    public function listen(string $name, callable $callback, int $rank, int $arity): void {
        $this->listeners[$name][] = [$callback, $rank, $arity];
        unset($this->queues[$name]);
    }

    private function queue(string $name) {
        if (!isset($this->listeners[$name])) { return []; }
        if (!isset($this->queues[$name])) {
            $queue = $this->listeners[$name];
            usort($queue, fn(array $a, array $b) => $a[1] <=> $b[1]);
            $this->queues[$name] = $queue;
        }
        return $this->queues[$name];
    }

    // Hands a value through every listener in turn, each receiving the last one's result and as much context as it takes.
    public function pass(string $name, mixed $value, array $context) {
        foreach ($this->queue($name) as $listener) {
            if (1 === $listener[2] || [] === $context) {
                $value = call_user_func($listener[0], $value);
            } else {
                $value = call_user_func_array($listener[0], [$value, ...array_slice($context, 0, $listener[2] - 1)]);
            }
        }
        return $value;
    }

    public function announce(string $name, array $context): void {
        $this->fired[$name] = ($this->fired[$name] ?? 0) + 1;
        foreach ($this->queue($name) as $listener) { call_user_func_array($listener[0], array_slice($context, 0, $listener[2])); }
    }
}

final class Cache {
    private array $groups = [];
    public int $hits = 0, $misses = 0;

    // A stored null is found as null; `$found` tells it apart from a key never stored.
    public function read(string $group, string $key, ?bool &$found = null) {
        $box = $this->groups[$group][$key] ?? null;
        $found = null !== $box;
        if (!$found) { $this->misses++; return null; }
        $this->hits++;
        return $box[0];
    }

    public function write(string $group, string $key, mixed $value): void { $this->groups[$group][$key] = [$value]; }
}

final class Person { public ?string $link = null; public function __construct(public int $id, public string $name) {} }

final class Entry {
    public string $path; public ?string $teaser = null, $lock = null; public ?array $labels = null;
    public ?Entry $parent = null; public ?Person $editor = null;
    public function __construct(public int $id, public string $title, public string $text, public Person $author, public string $kind,
        public int $stamp, public int $notes = 0, public string $state = 'live') { $this->path = "/{$kind}/{$id}"; }
}

// What a request asks for, read from its variables: one entry, a search, a label, an author, a month, or the newest entries.
final class Route {
    // The variables a request may carry and the type each must have; a value of another type is read as absent.
    private const SCHEMA = ['entry' => 'id', 'find' => 'text', 'label' => 'text', 'by' => 'id', 'year' => 'id', 'month' => 'id',
        'day' => 'id', 'page' => 'id', 'size' => 'id', 'sort' => 'text', 'order' => 'text', 'kind' => 'text', 'feed' => 'flag',
        'preview' => 'flag', 'embed' => 'flag', 'fields' => 'text', 'lang' => 'text', 'skin' => 'text'];
    public string $kind = 'newest';
    public ?int $entry = null, $author = null, $year = null;
    public ?string $term = null, $label = null;
    public array $vars = [], $words = [];
    public int $page = 1, $size = 10;
    public bool $single = false, $feed = false, $dated = false, $sorted = false;

    public function __construct(array $request) {
        $vars = [];
        foreach (self::SCHEMA as $name => $type) {
            $read = null;
            if (!isset($request[$name]) || null === $request[$name]) {
                $vars[$name] = $read;
                continue;
            }
            $value = $request[$name];
            if ('flag' === $type) {
                $read = true === $value || '1' === $value;
            } elseif ('id' === $type && (is_int($value) || (is_string($value) && ctype_digit($value)))) {
                $read = (int) $value;
            } elseif ('text' === $type && is_string($value) && !empty(trim($value))) {
                $read = trim($value);
            }
            $vars[$name] = $read;
        }
        $this->vars = $vars;
        if (null !== $this->vars['entry'] && $this->vars['entry'] > 0) { $this->entry = $this->vars['entry']; }
        if (null !== $this->vars['find']) { $this->term = $this->vars['find']; }
        if (null !== $this->vars['label']) { $this->label = strtolower($this->vars['label']); }
        if (null !== $this->vars['by']) { $this->author = $this->vars['by']; }
        if (null !== $this->vars['year']) { $this->year = $this->vars['year']; $this->dated = true; }
        if (null !== $this->vars['page'] && $this->vars['page'] > 1) { $this->page = $this->vars['page']; }
        if (null !== $this->vars['size'] && $this->vars['size'] > 0) { $this->size = min(50, $this->vars['size']); }
        $this->feed = true === $this->vars['feed'] && !$this->vars['embed'];
        $this->sorted = null !== $this->vars['sort'] && in_array($this->vars['sort'], ['stamp', 'title', 'notes'], true);
        if (null !== $this->entry) {
            $this->kind = 'entry';
            $this->single = true;
        } elseif (null !== $this->term) {
            $this->kind = 'search';
        } elseif (null !== $this->label || null !== $this->author || $this->dated) {
            $this->kind = null !== $this->label ? 'label' : (null !== $this->author ? 'author' : 'month');
        }
    }

    // Whether a panel listing by this route shows the entry.
    public function shows(Entry $entry) {
        if (!empty($this->vars['kind']) && $entry->kind !== $this->vars['kind']) { return false; }
        if (!empty($this->author) && $entry->author->id !== $this->author) { return false; }
        if (!empty($this->label) && !in_array($this->label, $entry->labels['topics'] ?? [], true)) { return false; }
        if (!$this->single && $this->dated && $entry->stamp < mktime(0, 0, 0, 1, 1, $this->year)) { return false; }
        return true;
    }

    public function query() {
        $which = $this->single ? 'id is ' . $this->entry : '(kind is post or kind is page)';
        $tests = ['state is live', 'not state is hidden', $which, 'stamp below 2000000000'];
        if (null !== $this->term) { $tests[] = "(title has '" . addslashes($this->term) . "' or text has '" . addslashes($this->term) . "')"; }
        if (null !== $this->label) { $tests[] = "labels has '" . $this->label . "'"; }
        if (null !== $this->author) { $tests[] = 'author is ' . $this->author; }
        $window = ' take ' . $this->size . ($this->page > 1 ? ' skip ' . ($this->page - 1) * $this->size : '');
        $order = $this->sorted ? $this->vars['sort'] : 'stamp';
        if ($this->feed) { $window = ' take ' . 2 * $this->size; }
        return 'find entries where ' . implode(' and ', $tests) . " sort {$order} down{$window}";
    }
}

final class Listing {
    public array $entries = [];
    public ?Entry $current = null;
    public int $shown = 0;
    public function __construct(public Site $site, public Route $route) {}

    public function next() {
        $this->current = $this->entries[$this->shown] ?? null;
        if (null === $this->current) { return false; }
        $this->shown++;
        emit('entry_start', $this->current, $this);
        return true;
    }

    public function card() {
        $entry = $this->current;
        $title = refine('entry_title', $this->current->title ?? '', $entry);
        $kind = $this->current->kind ?? 'post';
        $href = refine('entry_href', $this->current->path, $entry);
        $author = escape($this->current->author->name);
        $authorLink = $this->current->author->link ?? '';
        $editor = $this->current->editor->name ?? '';
        $parent = $this->current->parent->title ?? '';
        $labels = $this->current->labels['topics'] ?? [];
        $pinned = $this->current->labels['pinned'] ?? false;
        $full = $this->route->single && $this->current->id === $this->route->entry;
        $teaser = $this->current->teaser ?? first_words($this->current->text, 24);
        $body = '';
        if (null !== $this->current->lock && !$full) {
            $body = '<p class="locked">This entry is locked.</p>';
        } elseif ($full) {
            $body = refine('entry_text', $this->current->text, $entry);
        } else {
            $body = refine('entry_teaser', $teaser, $entry);
        }
        $notes = 0;
        if ($this->current->notes > 2) { $notes = $this->site->query("find notes where entry is {$this->current->id} take 50"); }
        $classes = refine('entry_classes', ['entry', 'state-' . ($this->current->state ?? 'live')], $entry);
        $day = date($this->site->setting('day_format', 'Y-m-d'), $this->current->stamp);
        $tagged = '';
        foreach ($labels as $label) {
            if (!is_string($label) || '' === $label) { continue; }
            $tagged .= '<a' . attributes(['href' => '/label/' . rawurlencode($label), 'rel' => 'tag'], ATTRIBUTES['a']) . '>' . escape($label) . '</a> ';
        }
        $byline = $author;
        if ('' !== $authorLink) { $byline = '<a href="' . escape($authorLink) . '">' . $author . '</a>'; }
        $editedBy = '';
        if ('' !== $editor) { $editedBy = ', edited by ' . escape($editor); }
        $under = '';
        if ('' !== $parent) { $under = ' under ' . escape($parent); }
        $article = ['id' => "{$kind}-{$this->current->id}", 'class' => implode(' ', $classes), 'data-pinned' => $pinned, 'data-notes' => $notes];
        $link = ['href' => $href, 'class' => 'title', 'onclick' => 'go()'];
        $html = '<article' . attributes($article, ATTRIBUTES['article']) . '><h2><a' . attributes($link, ATTRIBUTES['a']) . '>' . escape($title)
            . "</a></h2><p class=\"byline\">{$byline}{$editedBy} <time" . attributes(['datetime' => $day], ATTRIBUTES['time'])
            . ">{$day}</time>{$under}</p>\n{$body}";
        if ($this->current->notes > 0 || $this->site->setting('replies_open', false)) {
            $html .= "<a class=\"notes\" href=\"{$href}#notes\">{$this->current->notes} notes</a>";
        }
        emit('entry_end', $entry);
        return $html . $tagged . "</article>\n";
    }
}

final class Site {
    public Syntax $syntax;
    public int $queries = 0, $nodes = 0, $rejected = 0; public array $settings = [], $lexed = [];
    public function __construct() { $this->syntax = new Syntax(Lexer::KINDS, SYNTAX); }

    // Parses a query, splitting each text into tokens once, and returns how many tests it holds, or -1 when it does not parse.
    public function query(string $text) {
        $this->queries++;
        if (!isset($this->lexed[$text])) { $this->lexed[$text] = (new Lexer($text))->tokens(); }
        $parser = new Parser($this->syntax, $this->lexed[$text]);
        $tree = $parser->tree($this->syntax->kinds['query']);
        $this->nodes += $parser->built;
        if (null === $tree) { $this->rejected++; return -1; }
        return $parser->tests;
    }

    public function setting(string $name, mixed $fallback = '') {
        $found = false;
        $value = cached('settings', $name, $found);
        if (!$found) {
            $value = $this->settings[$name] ?? $fallback;
            $this->query("find settings where name is '" . $name . "' take 1");
            keep('settings', $name, $value);
        }
        if (!listening('setting:' . $name)) { return $value; }
        return refine('setting:' . $name, $value, $name);
    }
}

final class Theme {
    public int $headings = 0, $paragraphs = 0; public array $seen = [];

    public function heading(string $title, Entry $entry) { $this->headings++; return 0 === $entry->id % 7 ? $title . ' *' : $title; }

    public function typeset(string $text) {
        if (false === strpbrk($text, "-.'")) { return $text; }
        return strtr($text, ['---' => '&#8212;', '--' => '&#8211;', '...' => '&#8230;', "'" => '&#8217;']);
    }

    public function paragraphs(string $text) {
        $html = '';
        foreach (explode("\n\n", trim($text)) as $block) {
            if ('' === $block) { continue; }
            $this->paragraphs++;
            $html .= '<p>' . str_replace("\n", "<br>\n", $block) . "</p>\n";
        }
        return $html;
    }

    public function classes(array $classes, Entry $entry) {
        $classes[] = $entry->kind . '-' . $entry->id;
        foreach (['pinned', 'draft', 'featured'] as $flag) { if (!empty($entry->labels[$flag])) { $classes[] = $flag; } }
        if (null !== $entry->parent) { $classes[] = 'child'; }
        if (null !== $entry->lock) { $classes[] = 'locked'; }
        return $classes;
    }

    public function href(string $href, Entry $entry) {
        if (null === $entry->parent) { return $href; }
        return $href . '?under=' . $entry->parent->id;
    }
    public function tally(Entry $entry, Listing $listing): void {
        $what = "{$listing->route->kind} {$entry->kind}";
        $this->seen[$what] = ($this->seen[$what] ?? 0) + 1;
    }
    public function terms(Route $route): void { if (null !== $route->term) { $route->words = explode(' ', strtolower($route->term)); } }
    public static function https(string $url) {
        if (!str_starts_with($url, 'http://')) { return $url; }
        return 'https://' . substr($url, 7);
    }
}

function render_page(Site $site, Theme $theme, array $vars, array $entries) {
    $route = new Route($vars);
    emit('route_read', $route);
    $site->query($route->query());
    $ids = array_map(fn(Entry $entry) => $entry->id, $entries);
    if (null === cached('lists', implode(',', $ids))) {
        $site->query('find entries where id in (' . implode(', ', $ids) . ')');
        keep('lists', implode(',', $ids), true);
    }
    $listing = new Listing($site, $route);
    $related = 0;
    foreach ($entries as $entry) {
        $id = $entry->id;
        if ($route->single && $id !== $route->entry) {
            if ($related >= 8 || 'post' !== $entry->kind) { continue; }
            $related++;
        }
        if (null !== $route->term && !mentions($entry, $route->words)) { continue; }
        $found = false;
        $loaded = cached('entries', (string) $id, $found);
        if (!$found) {
            $site->query("find notes where entry is {$id} sort stamp up");
            keep('entries', (string) $id, $entry);
            $loaded = $entry;
        }
        $listing->entries[] = $loaded;
    }
    $html = '<!DOCTYPE html><html><head><title>' . escape($site->setting('name')) . '</title>';
    foreach (['base', 'layout', 'colours', 'print'] as $sheet) { $html .= stylesheet($site, $sheet); }
    $classes = refine('page_classes', [$route->kind, $route->single ? 'single' : 'list', null !== $route->term ? 'search' : 'browse']);
    $html .= '</head><body class="' . escape(implode(' ', $classes)) . '"><header><h1>' . escape($site->setting('name')) . '</h1><p>'
        . escape($site->setting('motto')) . "</p></header>\n";
    while ($listing->next()) { $html .= $listing->card(); }
    $html .= '<aside>';
    foreach (PANELS as $panel) { $html .= render_panel($site, $panel, $entries); }
    return refine('page_html', $html . '</aside><footer>' . escape($site->setting('footer', '')) . '</footer></body></html>');
}

function render_panel(Site $site, array $panel, array $entries) {
    [$heading, $vars, $query] = $panel;
    $route = new Route($vars);
    $found = false;
    $tests = cached('panels', $query, $found);
    if (!$found) { $tests = $site->query($query); keep('panels', $query, $tests); }
    $html = '<section class="panel"><h3>' . escape(refine('panel_heading', $heading, $route)) . '</h3><ul data-tests="' . $tests . '">';
    [$shown, $limit] = [0, $route->size];
    foreach ($entries as $entry) {
        if ($shown >= $limit) { break; }
        if (!$route->shows($entry)) { continue; }
        $href = refine('entry_href', $entry->path, $entry);
        $html .= '<li><a href="' . $href . '">' . escape($entry->title) . '</a></li>';
        $shown++;
    }
    return $html . '</ul></section>';
}

[$events, $cache, $site, $theme] = [new Events(), new Cache(), new Site(), new Theme()];
$site->settings = ['name' => "Shapes & Measures", 'motto' => 'Notes on "engines" <and> their loops', 'base_url' => 'http://127.0.0.1:8099',
    'footer' => 'Built for counting', 'day_format' => 'Y-m-d', 'replies_open' => true];
foreach ([['entry_title', [$theme, 'heading'], 10, 2], ['entry_title', 'trim', 20], ['entry_text', [$theme, 'typeset']],
    ['entry_text', [$theme, 'paragraphs'], 11], ['entry_teaser', [$theme, 'typeset']], ['entry_classes', [$theme, 'classes'], 10, 2],
    ['entry_href', [$theme, 'href'], 10, 2], ['entry_href', 'Theme::https', 12], ['asset_url', 'Theme::https'],
    ['entry_start', [$theme, 'tally'], 10, 2], ['route_read', [$theme, 'terms']],
    ['page_classes', fn(array $classes) => array_values(array_unique($classes))], ['setting:name', 'typographic_quotes'],
    ['page_html', fn(string $html) => $html . "\n"]] as $listener) { on(...$listener); }
$words = ['engine', 'loop', 'opline', 'frame', 'hello', 'kernel', 'shape', 'trace', 'cache', 'slot', 'arena', 'heap'];
$people = [new Person(1, 'Ada'), new Person(2, 'Grace'), new Person(3, "O'Neil <ed>")];
$people[0]->link = 'https://example.test/ada';
$entries = [];
for ($i = 1; $i <= 12; $i++) {
    $text = '';
    for ($p = 0; $p < 4; $p++) {
        $sentence = array_map(fn($w) => $words[($i * 7 + $p * 5 + $w * 3) % 12], range(0, 17));
        $text .= ucfirst(implode(' ', $sentence)) . "... it's -- a test.\n\n";
    }
    $kind = 0 === $i % 4 ? 'page' : 'post';
    $entry = new Entry($i, 'Entry ' . $i . ': ' . $words[$i % 12], $text, $people[$i % 3], $kind, 1700000000 + $i * 86400, $i % 5);
    if (0 === $i % 3) { $entry->teaser = 'A short teaser for entry ' . $i; }
    if (0 === $i % 2) { $entry->labels = ['topics' => [$words[$i % 12], $words[($i + 5) % 12]], 'pinned' => 4 === $i]; }
    if (0 === $i % 5) { $entry->editor = $people[($i + 1) % 3]; }
    if (9 === $i) { $entry->lock = 'secret'; }
    $entries[] = $entry;
}
$entries[5]->parent = $entries[2];

$requests = [[], ['entry' => 1], ['find' => 'hello']];
[$rounds, $sum, $bytes] = [240, 0, 0];
for ($round = 0; $round < $rounds; $round++) {
    foreach ($requests as $n => $request) {
        foreach (STARTUP as $name) { emit($name); }
        foreach (SETTINGS as $name) { $site->setting($name); }
        $out = render_page($site, $theme, $request, $entries);
        $bytes += strlen($out);
        $sum = crc32($sum . $out);
        if (0 === $round) { echo 'page ', $n, ': ', strlen($out), ' bytes, crc ', crc32($out), "\n"; }
    }
}
echo 'rounds ', $rounds, ', bytes ', $bytes, ', crc ', $sum, "\n";
echo 'queries ', $site->queries, ', rejected ', $site->rejected, ', nodes ', $site->nodes, "\n";
echo 'cache ', $cache->hits, ' hits, ', $cache->misses, " misses\n";
ksort($events->fired);
foreach ($events->fired as $name => $times) { echo $name, ' ', $times, "\n"; }
echo 'headings ', $theme->headings, ', paragraphs ', $theme->paragraphs, "\n";
foreach ($theme->seen as $what => $times) { echo $what, ' ', $times, "\n"; }

// The helpers, declared after their callers.

function mentions(Entry $entry, array $words) {
    $haystack = strtolower($entry->title . ' ' . $entry->text);
    foreach ($words as $word) {
        if (!is_string($word) || '' === $word) { continue; }
        if (!str_contains($haystack, $word)) { return false; }
    }
    return true;
}

function first_words(string $text, int $count) {
    $words = explode(' ', $text, $count + 1);
    if (!isset($words[$count])) { return $text; }
    unset($words[$count]);
    return implode(' ', $words) . '&hellip;';
}

// Keeps the attributes `$allowed` names, drops null, false and array values, and renders the rest as ` name="value"`.
function attributes(array $pairs, array $allowed) {
    $html = '';
    foreach ($pairs as $name => $value) {
        $kept = isset($allowed[$name]) && !is_array($value);
        if (!$kept || null === $value || false === $value) { continue; }
        $rule = $allowed[$name];
        if (true === $value) {
            $html .= ' ' . $name;
            continue;
        }
        $text = (string) $value;
        if (is_string($rule) && !str_starts_with($text, $rule)) { continue; }
        if (false !== strpbrk($text, "&<>\"'")) { $text = htmlspecialchars($text, ENT_QUOTES | ENT_HTML5, 'UTF-8'); }
        $html .= " {$name}=\"{$text}\"";
    }
    return $html;
}

function stylesheet(Site $site, string $sheet) {
    $href = refine('asset_url', $site->setting('base_url') . '/skin/' . $sheet . '.css', $sheet);
    $link = ['rel' => 'stylesheet', 'href' => $href, 'media' => 'print' === $sheet ? 'print' : 'all', 'id' => $sheet];
    return '<link' . attributes($link, ATTRIBUTES['link']) . ">\n";
}

function typographic_quotes(string $text) { return str_replace("'", '&#8217;', $text); }

function on(string $name, callable $callback, int $rank = 10, int $arity = 1) { global $events; $events->listen($name, $callback, $rank, $arity); }

function listening(string $name) { global $events; return isset($events->listeners[$name]); }

function cached(string $group, string $key, ?bool &$found = null) { global $cache; return $cache->read($group, $key, $found); }

function keep(string $group, string $key, mixed $value) { global $cache; $cache->write($group, $key, $value); }

function emit(string $name, mixed ...$context) { global $events; $events->announce($name, $context); }

function escape(string $text) {
    $special = '' !== $text && false !== strpbrk($text, "&<>\"'");
    return $special ? htmlspecialchars($text, ENT_QUOTES | ENT_HTML5, 'UTF-8') : $text;
}

function refine(string $name, mixed $value, mixed ...$context) {
    global $events;
    if (!isset($events->listeners[$name])) { return $value; }
    return $events->pass($name, $value, $context);
}
