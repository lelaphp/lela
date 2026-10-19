<?php
// docs/perf/workloads/nbody.php at $N = 6: the types golden's program, 5 for
// count($mass) and ** 0.5 for sqrt, and echo in place of printf, which an
// engine with no extension does not have.
// n-body — Jovian planets gravitational simulation.
// (Adapted from the Computer Language Benchmarks Game.)
//
// Exercises: dense float64 arithmetic, sqrt, and repeated array indexing inside
// a tight O(bodies^2) inner loop run N times. This is the "number crunching"
// test — it leans on the value representation and arithmetic fast paths.
//
// Layout note: a struct-of-arrays form (parallel flat arrays, not an array of
// records) so it only needs single-level element writes — portable across
// engines and the conventional high-performance n-body shape.
//
// Deterministic output: total energy before and after, to 9 decimals.

$N = 6; // @bench-param  (number of integration steps)

const PI          = 3.141592653589793;
const SOLAR_MASS  = 4.0 * PI * PI;
const DAYS_PER_YR = 365.24;

//        Sun  Jupiter              Saturn               Uranus               Neptune
$x  = [0.0,  4.84143144246472090e+00,  8.34336671824457987e+00,  1.28943695621391310e+01,  1.53796971148509165e+01];
$y  = [0.0, -1.16032004402742839e+00,  4.12479856412430479e+00, -1.51111514016986312e+01, -2.59193146099879641e+01];
$z  = [0.0, -1.03622044471123109e-01, -4.03523417114321381e-01, -2.23307578892655734e-01,  1.79258772950371181e-01];
$vx = [0.0,  1.66007664274403694e-03,  -2.76742510726862411e-03,  2.96460137564761618e-03,  2.68067772490389322e-03];
$vy = [0.0,  7.69901118419740425e-03,   4.99852801234917238e-03,  2.37847173959480950e-03,  1.62824170038242295e-03];
$vz = [0.0, -6.90460016972063023e-05,   2.30417297573763929e-05, -2.96589568540237556e-05, -9.51592254519715870e-05];
$mass = [
    SOLAR_MASS,
    9.54791938424326609e-04 * SOLAR_MASS,
    2.85885980666130812e-04 * SOLAR_MASS,
    4.36624404335156298e-05 * SOLAR_MASS,
    5.15138902046611451e-05 * SOLAR_MASS,
];

// velocities are given per year; convert to per day
for ($i = 1; $i < 5; $i++) {
    $vx[$i] *= DAYS_PER_YR;
    $vy[$i] *= DAYS_PER_YR;
    $vz[$i] *= DAYS_PER_YR;
}

function offset_momentum(array &$vx, array &$vy, array &$vz, array $mass): void
{
    $px = 0.0;
    $py = 0.0;
    $pz = 0.0;
    $n = 5;
    for ($i = 0; $i < $n; $i++) {
        $px += $vx[$i] * $mass[$i];
        $py += $vy[$i] * $mass[$i];
        $pz += $vz[$i] * $mass[$i];
    }
    $vx[0] = -$px / SOLAR_MASS;
    $vy[0] = -$py / SOLAR_MASS;
    $vz[0] = -$pz / SOLAR_MASS;
}

function energy(array $x, array $y, array $z, array $vx, array $vy, array $vz, array $mass): float
{
    $e = 0.0;
    $n = 5;
    for ($i = 0; $i < $n; $i++) {
        $e += 0.5 * $mass[$i] * ($vx[$i] * $vx[$i] + $vy[$i] * $vy[$i] + $vz[$i] * $vz[$i]);
        for ($j = $i + 1; $j < $n; $j++) {
            $dx = $x[$i] - $x[$j];
            $dy = $y[$i] - $y[$j];
            $dz = $z[$i] - $z[$j];
            $dist = ($dx * $dx + $dy * $dy + $dz * $dz) ** 0.5;
            $e -= ($mass[$i] * $mass[$j]) / $dist;
        }
    }
    return $e;
}

function advance(
    array &$x, array &$y, array &$z,
    array &$vx, array &$vy, array &$vz,
    array $mass, float $dt
): void {
    $n = 5;
    for ($i = 0; $i < $n; $i++) {
        for ($j = $i + 1; $j < $n; $j++) {
            $dx = $x[$i] - $x[$j];
            $dy = $y[$i] - $y[$j];
            $dz = $z[$i] - $z[$j];
            $d2 = $dx * $dx + $dy * $dy + $dz * $dz;
            $mag = $dt / ($d2 * $d2 ** 0.5);

            $mj = $mass[$j] * $mag;
            $mi = $mass[$i] * $mag;
            $vx[$i] -= $dx * $mj;
            $vy[$i] -= $dy * $mj;
            $vz[$i] -= $dz * $mj;
            $vx[$j] += $dx * $mi;
            $vy[$j] += $dy * $mi;
            $vz[$j] += $dz * $mi;
        }
    }
    for ($i = 0; $i < $n; $i++) {
        $x[$i] += $dt * $vx[$i];
        $y[$i] += $dt * $vy[$i];
        $z[$i] += $dt * $vz[$i];
    }
}

offset_momentum($vx, $vy, $vz, $mass);
echo energy($x, $y, $z, $vx, $vy, $vz, $mass), "\n";
for ($s = 0; $s < $N; $s++) {
    advance($x, $y, $z, $vx, $vy, $vz, $mass, 0.01);
}
echo energy($x, $y, $z, $vx, $vy, $vz, $mass), "\n";
