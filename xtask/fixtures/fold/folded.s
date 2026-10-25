# The positive control of xtask fold (#3384): lines 1-17, 1318-1330,
# 5858-5862 and 6469-6474 of objdump --disassemble-symbols over the hot
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
1012a187c:     	b.ls	0x1012a18f4 <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0x5be4>
1012a1880:     	mov	w11, #0x20              ; =32
1012a1884:     	add	x12, x19, #0x670
1012a1888:     	ldr	x11, [x12, x11]
1012a188c:     	mov	w12, #0x38              ; =56
1012a2208:     	stp	x10, x11, [x9]
1012a220c:     	cbz	x1, 0x1012a651c <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xa80c>
1012a2210:     	add	x0, x19, #0xf58
1012a2214:     	bl	0x101268fd8 <__ZN8php_heap4Heap14release_string17hae6e1a7d87c34062E>
1012a2218:     	b	0x1012a651c <__ZN10php_engine4tier11serve_frame17h44bf34a797a0c109E+0xa80c>
1012a221c:     	ldr	x9, [x19, #0x5c8]
