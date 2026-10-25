# A negative control of xtask fold (#3384): lines 1-17, 46-49, 110-119 and
# 10446-10452 of objdump --disassemble-symbols over the same instantiation
# in main e61851ebe's release php, without the option
# (base-sf1.s, on the ticket's build): x27 holds x19 + 0x920 apart
# and is loaded through, beside x28's x19 + 0x918, which escapes.

bin/php-base:	file format mach-o arm64

Disassembly of section __TEXT,__text:

00000001001e0db4 <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE>:
1001e0db4:     	stp	x28, x27, [sp, #-0x60]!
1001e0db8:     	stp	x26, x25, [sp, #0x10]
1001e0dbc:     	stp	x24, x23, [sp, #0x20]
1001e0dc0:     	stp	x22, x21, [sp, #0x30]
1001e0dc4:     	stp	x20, x19, [sp, #0x40]
1001e0dc8:     	stp	x29, x30, [sp, #0x50]
1001e0dcc:     	add	x29, sp, #0x50
1001e0dd0:     	sub	sp, sp, #0x3a0
1001e0dd4:     	mov	x20, x2
1001e0dd8:     	mov	x22, x1
1001e0ddc:     	mov	x19, x0
1001e0e50:     	str	x9, [sp, #0x88]
1001e0e54:     	add	x27, x19, #0x920
1001e0e58:     	add	x28, x19, #0x918
1001e0e5c:     	add	x8, x8, #0x140
1001e0f50:     	sub	x8, x21, #0x2
1001e0f54:     	cmp	x21, #0x1
1001e0f58:     	csinc	x8, x8, xzr, hi
1001e0f5c:     	cbnz	x8, 0x1001e1110 <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0x35c>
1001e0f60:     	ldur	x21, [x29, #-0x78]
1001e0f64:     	tbz	w26, #0x0, 0x1001e121c <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0x468>
1001e0f68:     	ldr	x10, [x27]
1001e0f6c:     	cmp	x10, x24
1001e0f70:     	b.ls	0x1001e0fbc <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0x208>
1001e0f74:     	ldr	x8, [x28]
1001eb0d0:     	ldr	x9, [x19, #0x928]
1001eb0d4:     	cmp	x8, x9
1001eb0d8:     	b.ne	0x1001eb140 <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0xa38c>
1001eb0dc:     	add	x0, x19, #0x918
1001eb0e0:     	bl	0x10160bbd4 <__ZN8php_heap5stack6frames15Frames$LT$H$GT$4grow17hce1896af0dd227cfE>
1001eb0e4:     	b	0x1001f0f94 <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0x101e0>
1001eb0e8:     	lsl	x1, x23, #4
