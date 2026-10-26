# The positive control of xtask fold (#3384): lines 1-17, 112-121 and
# 10446-10452 of objdump --disassemble-symbols over serve_frame's
# instantiation 17h931dc31e5153ce4d in this branch's release php at
# 6f4c7cbe1, with --cgp-max-address-users-to-scan=1000
# (head-sf1.s, on the ticket's build): the frame table's second word
# folded, [x19, #0x920], and its first, x19 + 0x918, handed to
# Frames::grow and so reached through an add, escaped.

bin/php-head:	file format mach-o arm64

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
1001e0f58:     	ldur	x21, [x29, #-0x78]
1001e0f5c:     	tbz	w26, #0x0, 0x1001e121c <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0x468>
1001e0f60:     	ldr	x10, [x19, #0x920]
1001e0f64:     	cmp	x10, x24
1001e0f68:     	b.ls	0x1001e0fb8 <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0x204>
1001e0f6c:     	add	x8, x19, #0x918
1001e0f70:     	ldr	x8, [x8]
1001e0f74:     	lsl	x9, x24, #5
1001e0f78:     	ldr	x8, [x8, x9]
1001e0f7c:     	ldur	w8, [x8, #-0x28]
1001eb0d0:     	ldr	x9, [x19, #0x928]
1001eb0d4:     	cmp	x8, x9
1001eb0d8:     	b.ne	0x1001eb140 <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0xa38c>
1001eb0dc:     	add	x0, x19, #0x918
1001eb0e0:     	bl	0x101609ce8 <__ZN8php_heap5stack6frames15Frames$LT$H$GT$4grow17hce1896af0dd227cfE>
1001eb0e4:     	b	0x1001f0ed4 <__ZN10php_engine4tier11serve_frame17h931dc31e5153ce4dE+0x10120>
1001eb0e8:     	lsl	x1, x23, #4
