# The positive control of xtask fold (#3384): lines 1-25, 1318-1360,
# 5855-5866 and 6466-6478 of objdump --disassemble-symbols over the hot
# serve_frame instantiation of t3345's x1 build at 037f359d8, with
# --cgp-max-address-users-to-scan=1000 (_coordinator/s53d3/runs/t3345/x1-sf.s):
# the frame table's words folded, [x19, #0x5c8], a register-offset load
# through x19 + 0x670 and a call's argument at x19 + 0xf58.

bin/php-x1:	file format mach-o arm64

Disassembly of section __TEXT,__text:

000000010129bd10 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E>:
10129bd10:     	stp	x28, x27, [sp, #-0x60]!
10129bd14:     	stp	x26, x25, [sp, #0x10]
10129bd18:     	stp	x24, x23, [sp, #0x20]
10129bd1c:     	stp	x22, x21, [sp, #0x30]
10129bd20:     	stp	x20, x19, [sp, #0x40]
10129bd24:     	stp	x29, x30, [sp, #0x50]
10129bd28:     	add	x29, sp, #0x50
10129bd2c:     	sub	sp, sp, #0x410
10129bd30:     	str	xzr, [sp]
10129bd34:     	mov	x23, x1
10129bd38:     	mov	x19, x0
10129bd3c:     	movi.2d	v0, #0000000000000000
10129bd40:     	add	x8, sp, #0x1a
10129bd44:     	stur	q0, [x8, #0xff]
10129bd48:     	stp	q0, q0, [sp, #0x100]
10129bd4c:     	add	x8, sp, #0x130
10129bd50:     	stp	q0, q0, [sp, #0xe0]
10129bd54:     	add	x20, x8, #0x1c0
10129bd58:     	add	x9, x8, #0x80
10129d18c:     	ldr	x8, [x19, #0x5d0]
10129d190:     	mov	w25, w21
10129d194:     	cmp	x8, x25
10129d198:     	b.ls	0x1012a2d50 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x7040>
10129d19c:     	ldr	x8, [x19, #0x5c8]
10129d1a0:     	add	x8, x8, x25, lsl #5
10129d1a4:     	ldr	x10, [x8, #0x8]
10129d1a8:     	ldr	w9, [x23, #0x4]
10129d1ac:     	cmp	x10, x9
10129d1b0:     	b.ls	0x1012a2d50 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x7040>
10129d1b4:     	ldr	x8, [x8]
10129d1b8:     	add	x9, x8, x9, lsl #4
10129d1bc:     	ldrb	w8, [x9, #0x8]
10129d1c0:     	cmp	w8, #0x3
10129d1c4:     	b.gt	0x1012a2a50 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x6d40>
10129d1c8:     	sub	w9, w8, #0x1
10129d1cc:     	cmp	w9, #0x2
10129d1d0:     	b.hs	0x1012a2d48 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x7038>
10129d1d4:     	ldr	w8, [x23, #0x8]
10129d1d8:     	subs	w24, w8, w22
10129d1dc:     	b.hi	0x10129c574 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x864>
10129d1e0:     	ldr	x9, [x19, #0x310]
10129d1e4:     	ldrb	w9, [x9, #0x18]
10129d1e8:     	cbz	w9, 0x10129c574 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x864>
10129d1ec:     	ldr	x9, [x19, #0x5d0]
10129d1f0:     	cmp	x9, x25
10129d1f4:     	b.ls	0x10129d208 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x14f8>
10129d1f8:     	ldr	x9, [x19, #0x5c8]
10129d1fc:     	lsl	x10, x25, #5
10129d200:     	ldr	x9, [x9, x10]
10129d204:     	stur	w8, [x9, #-0x28]
10129d208:     	sub	x8, x29, #0xe0
10129d20c:     	mov	x0, x19
10129d210:     	bl	0x1002cf978 <__ZN10php_engine9interrupt11interrupted17he398a856fa91653bE>
10129d214:     	b	0x1012a3128 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x7418>
10129d218:     	ldr	x9, [x19, #0x5d0]
10129d21c:     	mov	w8, w21
10129d220:     	cmp	x9, x8
10129d224:     	b.ls	0x10129d238 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x1528>
10129d228:     	ldr	x9, [x19, #0x5c8]
10129d22c:     	lsl	x8, x8, #5
10129d230:     	ldr	x8, [x9, x8]
10129d234:     	stur	w22, [x8, #-0x28]
1012a1870:     	b	0x1012aa924 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xec14>
1012a1874:     	ldr	x11, [x19, #0x698]
1012a1878:     	cmp	x11, x10
1012a187c:     	b.ls	0x1012a18f4 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x5be4>
1012a1880:     	mov	w11, #0x20              ; =32
1012a1884:     	add	x12, x19, #0x670
1012a1888:     	ldr	x11, [x12, x11]
1012a188c:     	mov	w12, #0x38              ; =56
1012a1890:     	umaddl	x11, w10, w12, x11
1012a1894:     	ldr	x10, [x11, #0x18]
1012a1898:     	cbz	x10, 0x1012a18f4 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x5be4>
1012a189c:     	ldr	x11, [x11, #0x20]
1012a21fc:     	add	x8, x10, x12, lsl #4
1012a2200:     	ldp	x10, x11, [x8]
1012a2204:     	stp	xzr, xzr, [x8]
1012a2208:     	stp	x10, x11, [x9]
1012a220c:     	cbz	x1, 0x1012a651c <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xa80c>
1012a2210:     	add	x0, x19, #0xf58
1012a2214:     	bl	0x101268fd8 <__ZN8php_heap4Heap14release_string17hae6e1a7d87c34062E>
1012a2218:     	b	0x1012a651c <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xa80c>
1012a221c:     	ldr	x9, [x19, #0x5c8]
1012a2220:     	lsl	x8, x8, #5
1012a2224:     	ldr	x8, [x9, x8]
1012a2228:     	stur	w22, [x8, #-0x28]
1012a222c:     	mov	x0, x19
