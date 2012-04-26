//
// See issue #1535

enum a_tag {
    a_tag(u32)
}

type t_rec = {
    c8: u8,
    t: a_tag
};

fn main() {
    let x = {c8: 22u8, t: a_tag(44u32)};
    let y = #fmt["%?", x];
    #debug["y = %s", y];
    assert y == "(22, a_tag(44))";
}
