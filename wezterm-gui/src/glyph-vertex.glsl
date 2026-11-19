// This is the Glyph vertex shader.
// It is responsible for placing the glyph images in the
// correct place on screen.

precision highp float;

in vec2 position;
in vec2 tex;
in vec4 fg_color;
in float has_color;
in float mix_value;
in vec3 hsv;
in vec4 alt_color;

uniform mat4 projection;
uniform vec3 foreground_text_hsb;

out float o_has_color;
out vec2 o_tex;
out vec3 o_hsv;
out vec4 o_fg_color;
out vec4 o_fg_color_alt;
out float o_fg_color_mix;

void pass_through_vertex() {
  o_tex = tex;
  o_has_color = has_color;
  o_fg_color = fg_color;
  o_fg_color_alt = alt_color;
  o_fg_color_mix = mix_value;
  o_hsv = hsv;
}

void main() {
  pass_through_vertex();

  // Solids and monochrome glyphs have one constant RGB color per quad.
  // Convert neutral, unmixed colors once per vertex instead of running
  // three pow() operations for every pixel on software renderers.
  if ((has_color == 3.0 || (has_color == 0.0 && foreground_text_hsb == vec3(1.0)))
      && hsv == vec3(1.0) && mix_value == 0.0) {
    bvec3 cutoff = lessThan(fg_color.rgb, vec3(0.0031308));
    vec3 higher = vec3(1.055) * pow(fg_color.rgb, vec3(1.0/2.4)) - vec3(0.055);
    vec3 lower = fg_color.rgb * vec3(12.92);
    o_fg_color = vec4(mix(higher, lower, cutoff), fg_color.a);
    o_has_color = has_color == 3.0 ? 5.0 : 6.0;
  }

  // Use the adjusted cell position to render the quad
  gl_Position = projection * vec4(position, 0.0, 1.0);
}
