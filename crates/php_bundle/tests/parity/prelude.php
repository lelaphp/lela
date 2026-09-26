function proto_tally(object $o, ?int $set = null): int { static $m; $m ??= new WeakMap(); if ($set !== null) { $m[$o] = $set; } return $m[$o] ?? 0; }
function proto_raw($n) { if ($n === null) { throw new ValueError('proto_raw(): Argument #1 ($n) must not be null here'); } if (is_array($n)) { throw new TypeError('proto_raw(): Argument #1 ($n) must be of type int, array given'); } return (int) $n; }
function proto_seen(bool $flag = true): string { return func_num_args() ? 'given ' . var_export($flag, true) : 'omitted'; }
function proto_call(callable $callback, mixed ...$args): mixed { return $callback(...$args); }
function proto_count(iterable $of): int { return count($of); }
function proto_manual(int ...$n): string { if (!in_array(count($n), [0, 2], true)) { throw new ArgumentCountError('proto_manual() expects exactly 0 or 2 arguments'); } return '[' . implode(', ', $n) . ']'; }
function proto_describe(mixed $value): string { return (string) $value; }
function proto_never(): never { throw new Exception('never returns'); }
function proto_false(): false { return false; }
const PROTO_BUILT = 'at startup';
define('Proto\INSIDE', true);
enum ProtoLevel: int { case Low = 1; case High = 2; const int TOP = 2; }
interface ProtoCounter { public function value(): int; }
abstract class ProtoBase { abstract public function grow(): void; public function kind(): string { return 'base'; } }
final class ProtoBox extends ProtoBase implements ProtoCounter {
    const int START = 10;
    protected const int STEP = 5;
    const int|array MIXED = 7;
    const array LIST = [1, 'k' => 'v'];
    public string $label = 'box';
    protected ?string $tag = null;
    public array $list = [];
    public readonly int $id;
    public function __construct(int $start = self::START) { proto_tally($this, $start); }
    public function value(): int { return proto_tally($this); }
    public function grow(): void { proto_tally($this, proto_tally($this) + self::STEP); }
    public function add(int $n): static { proto_tally($this, proto_tally($this) + $n); return $this; }
    public static function make(int $n = 0): ProtoBox { $b = new ProtoBox(); proto_tally($b, $n); return $b; }
    protected function hidden(): void {}
    final public function done(): true { return true; }
}
class ProtoTally { public function value(): int { return proto_tally($this); } }
class ProtoLaid { public function same(): bool { return true; } }
