﻿package {
	public class Test {
	}
}

function assert_array(a) {
	for (var i = 0; i < a.length; i += 1) {
		trace(a[i]);
	}
}

trace("//var a = new Array(5,3,1,9,16)");
var a = new Array(5,3,1,9,16);

trace("//trace(a.every(function (val) { return val === 5; }));");
trace(a.every(function (val) {
	return val === 5;
}));

trace("//trace(a.every(function (val) { return val !== 20; }));");
trace(a.every(function (val) {
	return val !== 20;
}));

trace("//var b = new Array();");
var b = new Array();

trace("//trace(b.every(function (val) { return val === 5; }));");
trace(b.every(function (val) {
	return val === 5;
}));