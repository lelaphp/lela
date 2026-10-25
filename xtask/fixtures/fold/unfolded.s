# The negative control of xtask fold (#3384): lines 1-25 and 830-870 of
# objdump --disassemble-symbols over the hot serve_frame instantiation of
# t3345's c1 build at 037f359d8, without --cgp-max-address-users-to-scan
# (_coordinator/s53d3/runs/t3345/c1-sf.s): x27 and x28 hold the frame
# table's two words apart from x19.

bin/php-c1:	file format mach-o arm64

Disassembly of section __TEXT,__text:

000000010129c514 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E>:
10129c514:     	stp	x28, x27, [sp, #-0x60]!
10129c518:     	stp	x26, x25, [sp, #0x10]
10129c51c:     	stp	x24, x23, [sp, #0x20]
10129c520:     	stp	x22, x21, [sp, #0x30]
10129c524:     	stp	x20, x19, [sp, #0x40]
10129c528:     	stp	x29, x30, [sp, #0x50]
10129c52c:     	add	x29, sp, #0x50
10129c530:     	sub	sp, sp, #0x410
10129c534:     	str	xzr, [sp]
10129c538:     	mov	x23, x1
10129c53c:     	mov	x19, x0
10129c540:     	movi.2d	v0, #0000000000000000
10129c544:     	add	x8, sp, #0x1a
10129c548:     	stur	q0, [x8, #0xff]
10129c54c:     	stp	q0, q0, [sp, #0x100]
10129c550:     	add	x8, sp, #0x130
10129c554:     	add	x20, x8, #0x1c0
10129c558:     	add	x9, x8, #0x80
10129c55c:     	str	x9, [sp, #0x98]
10129d1f0:     	cbnz	w8, 0x10129d220 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xd0c>
10129d1f4:     	ldr	w8, [x23, #0x10]
10129d1f8:     	ldr	w3, [x24, #0x10]
10129d1fc:     	and	w2, w8, #0xfffffffc
10129d200:     	mov	x0, x19
10129d204:     	mov	x1, x21
10129d208:     	bl	0x100178ee4 <__ZN10php_engine3api7request6frames51_$LT$impl$u20$php_engine..api..request..Request$GT$15cached_property17he31c6fa5e6dc44f1E>
10129d20c:     	and	x8, x0, #0xffff
10129d210:     	cmp	x8, #0x2
10129d214:     	add	x27, x19, #0x5c8
10129d218:     	add	x28, x19, #0x5d0
10129d21c:     	b.ne	0x1012a35b0 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x709c>
10129d220:     	ldr	x8, [x28]
10129d224:     	cmp	x8, x25
10129d228:     	b.ls	0x10129d23c <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xd28>
10129d22c:     	ldr	x8, [x27]
10129d230:     	lsl	x9, x25, #5
10129d234:     	ldr	x8, [x8, x9]
10129d238:     	stur	w22, [x8, #-0x28]
10129d23c:     	mov	x0, x19
10129d240:     	mov	x1, x21
10129d244:     	mov	x2, x20
10129d248:     	mov	x3, x23
10129d24c:     	bl	0x10028bc68 <__ZN10php_engine8handlers11fetched_obj17h97de3c92c65520dcE>
10129d250:     	b	0x1012ad378 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x10e64>
10129d254:     	ldr	x9, [x28]
10129d258:     	mov	w8, w21
10129d25c:     	cmp	x9, x8
10129d260:     	b.ls	0x10129d274 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xd60>
10129d264:     	ldr	x9, [x27]
10129d268:     	lsl	x8, x8, #5
10129d26c:     	ldr	x8, [x9, x8]
10129d270:     	stur	w22, [x8, #-0x28]
10129d274:     	mov	x0, x19
10129d278:     	mov	x1, x21
10129d27c:     	mov	x2, x20
10129d280:     	mov	x3, x23
10129d284:     	bl	0x1012c11f4 <__ZN10php_engine8handlers11fetch_class17h22afb6990e85fe15E>
10129d288:     	b	0x1012ad378 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x10e64>
10129d28c:     	ldrb	w10, [x23, #0x3]
10129d290:     	ldr	w24, [x23, #0xc]
