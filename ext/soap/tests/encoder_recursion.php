<?php
class EncoderClient extends SoapClient {
    public $calls = 0;
    function __doRequest($r, $l, $a, $v, $o = false, $u = null): ?string {
        $this->calls++;
        return '<e:Envelope xmlns:e="http://schemas.xmlsoap.org/soap/envelope/"><e:Body><r><v>ok</v></r></e:Body></e:Envelope>';
    }
}
$client = new EncoderClient(null, ['location' => 'test://', 'uri' => 'urn:t', 'trace' => true]);
$value = 'leaf';
for ($i = 0; $i < $depth; $i++) {
    if ($shape === 'array') { $value = [$value]; }
    elseif ($shape === 'object') { $value = (object)['next' => $value]; }
    else { $value = new SoapVar((object)['next' => $value], SOAP_ENC_OBJECT); }
}
$fiber = new Fiber(function () use ($client, $value) {
    try {
        echo $client->__soapCall('f', [$value]), "\n";
    } catch (Error $e) {
        echo str_starts_with($e->getMessage(), 'Maximum call stack size of ')
            && str_ends_with($e->getMessage(), ' bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?')
            ? "floor\n" : $e->getMessage();
    }
});
$fiber->start();
unset($fiber);
echo 'calls:', $client->calls, "\n";
echo $client->__soapCall('f', ['small']), "\n";
echo str_contains($client->__getLastRequest(), 'small') ? "trace\n" : "bad trace\n";
while ($depth-- > 0) {
    if ($shape === 'array') { $value = $value[0]; }
    elseif ($shape === 'object') { $next = $value->next; unset($value->next); $value = $next; }
    else { $next = $value->enc_value->next; unset($value->enc_value->next); $value = $next; }
}
echo "clean\n";
