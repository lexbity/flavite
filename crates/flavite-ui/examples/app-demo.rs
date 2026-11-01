//! Visual demonstration of the Flavite application shell.

use flavite_ui::accessibility::program::FlaviteProgram;

fn main() -> Result<(), iced_winit::Error> {
    iced_winit::run(FlaviteProgram)
}
