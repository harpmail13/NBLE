# NBLE 1.0 Programming Language Documentation

**NBLE 1.0** is a compact, 100% whitespace-agnostic scripting language backed by a compiled Rust runtime. Designed for both ultra-dense single-line scripting and readable block-structured programs, every command begins with a distinct uppercase identifier, enabling zero-delimiter or spaced parsing while supporting AST loop fusion, PEMDAS math evaluation, binary persistence, OS-level multithreading, and shell execution (`Sy`).

---

## 1. Interactive Shell & CLI Commands (`nble1.0.exe`)

Launching `nble1.0.exe` with no arguments opens the standalone **NBLE 1.0 Interactive Shell** (`nble1.0>`). All shell commands can also be passed directly via CLI flags:

| Command | Syntax | Description |
| :--- | :--- | :--- |
| **`run`** | `run <path.nble>` | Parses and executes an `.nble` script file in the active session. |
| **`build`** | `build <Name> ~<code>~ @<Dir>` | Validates syntax first; if valid, writes `<Name>.nble` to `<Dir>`. Aborts without writing if syntax is invalid. |
| **`valid`** | `valid <path.nble>` | Parses and validates an `.nble` file without executing it, reporting top-level statement count. |
| **`condense`** | `condense <path.nble>` | Validates syntax, strips comments and whitespace, and outputs `<Name>condensed.nble`. |
| **`reset`** | `reset` | Cancels active threads and clears all global/local variables, functions, and dictionaries in memory. |
| **`help` / `exit`** | `help` / `exit` / `quit` | Displays shell commands or exits the interactive shell. |

---

## 2. Memory & Storage Registers

NBLE 1.0 supports both numeric-indexed and double-quoted named identifiers across global and thread-local scopes[cite: 1]:

| Prefix | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| **`V<n>` / `V"<name>"`** | Global Scalar | `Null` | Stores integers, floats, booleans, or strings shared across all threads. `V0` stores caught exception text[cite: 1]. |
| **`Vl<n>` / `Vl"<name>"`** | Thread-Local Scalar | `Null` | Isolated per OS thread (`Th`) and function call; never collides across concurrent workers. |
| **`D<n>`** | Dictionary / List | `[]` | 1-indexed dynamic container (`D0`–`D63`) supporting mixed types and binary disk serialization[cite: 1]. |
| **`F<n>` / `F"<name>"`** | Global Function | *None* | Stores a block `{...}` or legacy math pipeline (`F0=+1/2`). Local variants use `Fl<n>` / `Fl"<name>"`[cite: 1]. |
| **`Th("<id>")`** | Thread Handle | *None* | Spawns and tracks concurrent OS threads indexed by string or numeric ID[cite: 1]. |

---

## 3. Complete A–Z Command & Operator Examples

Every feature in NBLE 1.0 is listed below with a runnable code example and expected output[cite: 1].

### **`A` — Append to Dictionary**
Appends a value to dictionary `D<n>` at the next sequential 1-based integer index (`1`, `2`, `3`...)[cite: 1].
```text
V1 = "Sword"
A V1 D1
A "Shield" D1
P G1(D1)
P G2(D1)
? Output:
Sword
Shield ?
```

### **`Cf` — Cycle Fixed (Count Loop)**
Repeats a braced block a fixed number of times (`<count>` can be a literal, variable, or expression).
```text
V1 = 0
Cf(3) {
    V1 = V1 + 10
    P V1
}
? Output:
10
20
30 ?
```

### **`Cw` — Cycle While (While Loop)**
Repeats a braced block while the condition evaluates to true. Simple integer counter loops automatically fuse into native Rust loops at parse time[cite: 1].
```text
V1 = 1
Cw V1 E<= 3 {
    P V1
    V1 = V1 + 1
}
? Output:
1
2
3 ?
```

### **`D` — Dictionary (`D0`–`D63`)**
1-indexed container (`D0` through `D63`) that initializes empty and grows dynamically on append[cite: 1].
```text
A "First" D1
A "Second" D1
P G2(D1)
? Output:
Second ?
```

### **`E` — Equalities (`E`, `E!=`, `E<`, `E>`, `E>=`, `E<=`)**
Comparison operators used inside `I`, `J`, and `Cw` (`E=>` and `E=<` are accepted aliases for `E>=` and `E<=`)[cite: 1].
```text
V1 = 10
I V1 E 10 { P "Equal" }
I V1 E!= 5 { P "Not equal to 5" }
I V1 E> 5 { P "Greater than 5" }
I V1 E< 20 { P "Less than 20" }
I V1 E>= 10 { P "At least 10" }
I V1 E<= 10 { P "At most 10" }
```

### **`F` / `Fl` — Function (Block & Legacy Math Pipeline)**
Defines a reusable code block (`F "name" { ... }`) or sequential math chain (`F0 = +1/2`)[cite: 1]. Arguments passed in `(...)` populate `Vl0`, and the final value of `Vl0` is returned when called in an expression[cite: 1].
```text
? Block Function ?
F "double" {
    Vl0 = Mf(Vl0 * 2)
}
P F "double"(6)

? Legacy Math Pipeline ?
F0 = +1 /2
V1 = F0(9)
P V1
? Output:
12
5 ?
```

### **`G` — Get (Dictionary Item)**
Pulls the item at 1-based index `<idx>` from dictionary `D<n>`[cite: 1].
```text
A "Apple" D1
A "Banana" D1
V1 = 2
P GV1(D1)
? Output:
Banana ?
```

### **`H` — Else**
Executes if the preceding `I` condition (and all `J` conditions) evaluate to false[cite: 1].
```text
V1 = 3
I V1 E 10 {
    P "Ten"
} H {
    P "Not ten"
}
? Output:
Not ten ?
```

### **`I` — If & Standard Input**
* **Statement (`I <cond> { ... }`):** Evaluates a conditional branch[cite: 1].
* **Expression (`V1 = I`):** Reads a line from standard input (`stdin`), automatically parsing integers and floats when numeric.
```text
P "Enter a number:"
V1 = I
I V1 E> 0 {
    P "Positive number"
}
```

### **`Im` — Import Module**
Loads and executes another `.nble` script file once (automatically deduplicated by canonical file path).
```text
Im("math_helpers.nble")
P "Module loaded!"
```

### **`J` — Else If**
Chained after an `I { ... }` block to test additional conditions in order[cite: 1].
```text
V1 = 85
I V1 E>= 90 {
    P "Grade: A"
} J V1 E>= 80 {
    P "Grade: B"
} H {
    P "Grade: C or lower"
}
? Output:
Grade: B ?
```

### **`K` — Catch / Except**
Follows a `T { ... }` block. Catches runtime exceptions, stores the error message in `V0`, and executes `{ ... }`[cite: 1].
```text
T {
    V1 = 10 / 0
} K {
    P "Caught an error:"
    P V0
}
? Output:
Caught an error:
Division by zero ?
```

### **`L` — Load / Save Binary (`Ls` / `Lp`)**
Serializes (`Ls` = save) or deserializes (`Lp` = load) dictionary `D<n>` to `nble<filename>.bin` in the script directory[cite: 1].
```text
A "Level 5" D1
A "100 Gold" D1
Ls D1 "savegame"
Z D1
Lp D1 "savegame"
P G2(D1)
? Output (restored from nblesavegame.bin):
100 Gold ?
```

### **`M` — Math, PEMDAS (`Mf`), Angle Mode (`Mh`), Trig & Rounding (`Ms`, `Mc`, `Mt`, `Ma`, `Mr`)**
* `Mf(...)`: Evaluates expressions using strict PEMDAS order of operations with nested parentheses.
* `Mh = radians` / `Mh = degrees`: Switches trig angle mode (defaults to `degrees`).
* `Ms`, `Mc`, `Mt`, `Ma`, `Mr`: Sine, cosine, tangent, arcsine, and 4-decimal rounding[cite: 1].
* `M`: Infix modulo operator (`10 M 3` -> `1`).
```text
P Mf((2 + 3) * 4 ^ 2)
P Ms(30)
P Mc(60)
P Mt(45)
P Ma(0.5)
P Mr(3.1415926)
? Output:
80
0.49999999999999994
0.5000000000000001
0.9999999999999999
30.000000000000004
3.1416 ?
```

### **`P` — Print**
Prints a string, variable, or evaluated expression to standard output[cite: 1].
```text
V1 = 42
P "The answer is: " + V1
? Output:
The answer is: 42 ?
```

### **`R` / `Rs` — Random Integer**
Generates a random integer between `<min>` and `<max>` inclusive[cite: 1]. `R` uses a fast thread-local PRNG; `Rs` uses cryptographically secure OS randomness (`OsRng`).
```text
R V1, 1, 6
Rs Vl "secure_die", 1, 6
P V1
P Vl "secure_die"
```

### **`S` — Sleep (Wait)**
Pauses execution of the current thread for the specified number of seconds (supports integers, floats, or expressions)[cite: 1].
```text
P "Wait 0.5 seconds..."
S 0.5
P "Done!"
```

### **`Sy` — System Shell Execution**
Executes an operating system command (`cmd /C` on Windows, `sh -c` on Unix). Used as a statement (`Sy("...")`) to stream output directly, or as an expression (`V1 = Sy("...")`) to capture `stdout` as a string.
```text
V "host" = Sy("hostname")
P "Running on: " + V "host"
Sy("echo Direct shell execution!")
```

### **`T` — Try**
Wraps code that may raise a runtime error so execution can recover cleanly inside `K { ... }`[cite: 1].
```text
T {
    P G99(D1)
} K {
    P "Handled out-of-bounds read: " + V0
}
```

### **`Ti` / `Tiu` — Time Functions**
Returns timing values as expressions: `Ti` returns elapsed runtime seconds since startup, and `Tiu` returns the current Unix epoch timestamp in seconds[cite: 1].
```text
V1 = Ti
S 0.25
V2 = Mr(Ti - V1)
P "Elapsed seconds: " + V2
P "Unix timestamp: " + Tiu
```

### **`Th` / `The` / `Tht` — Multithreading**
* `Th("<id>") { ... }`: Spawns a background OS thread with isolated thread-local memory (`Vl`, `Fl`)[cite: 1].
* `The("<id>")`: Signals thread `<id>` to cancel and stop gracefully.
* `Tht("<id>")`: **Tail Thread Handoff** — moves all remaining statements in the current block onto a new background thread `<id>`.
```text
Th("worker1") {
    S 0.2
    P "Background thread finished!"
}
P "Main thread running!"
```

### **`V` / `Vl` — Variable**
Stores numbers, booleans, or strings using numeric (`V0`, `Vl0`) or named (`V "count"`, `Vl "temp"`) identifiers[cite: 1].
```text
V1 = "NBLE"
V "version" = 1.0
P V1 + " " + V "version"
? Output:
NBLE 1.0 ?
```

### **`W` — Where (Reverse Dictionary Search)**
Searches `D<n>` for a matching value and returns its 1-based index, or `0` if not found[cite: 1].
```text
A "Red" D1
A "Blue" D1
A "Green" D1
V1 = W("Blue", D1)
V2 = W("Yellow", D1)
P V1
P V2
? Output:
2
0 ?
```

### **`Z` — Zero / Clear Dictionary**
Clears all elements in dictionary `D<n>` back to empty[cite: 1].
```text
A "One" D1
A "Two" D1
Z D1
P W("One", D1)
? Output:
0 ?
```

---

## 4. Arithmetic Operators, Literals & Comments

| Symbol | Name | Example | Output / Notes |
| :--- | :--- | :--- | :--- |
| **`+`** | Add / Concatenate | `P 10 + 5`<br>`P "Hi " + "World"` | `15`<br>`Hi World`[cite: 1] |
| **`-`** | Subtract | `P 10 - 4` | `6`[cite: 1] |
| **`*`** | Multiply | `P 6 * 7` | `42`[cite: 1] |
| **`/`** | Divide | `P 15 / 2` | `7.5`[cite: 1] |
| **`%` / `M`** | Modulo | `P 15 % 4` | `3` |
| **`^`** | Exponent | `P 2 ^ 8` | `256`[cite: 1] |
| **`""`** | String Literal | `V1 = "Text"` | Supports `\n`, `\t`, `\r`, `\"`, `\\` escapes[cite: 1] |
| **`??`** | Comment | `? Ignored ?` | Single-line or multi-line comments[cite: 1] |

---

## 5. Complete Sample Program: Triggy 1.0 (NBLE 1.0 Edition)

### Formatted & Annotated Version
```text
? Triggy 1.0: Right-Triangle Trigonometry Calculator (NBLE 1.0) ?
Mh = degrees
V "running" = 1

Cw V "running" E 1 {
    P "=== Welcome To Triggy 1.0 ==="
    P "1. normal_trig (finding sides)"
    P "2. reverse_trig (finding angles & ratios)"
    P "e. Exit"
    P "Type 1, 2, or e to pick:"
    V "choice" = I

    I V "choice" E "e" {
        P "Exiting Triggy 1.0. Goodbye!"
        V "running" = 0
    } J V "choice" E 1 {
        T {
            P "Enter the known angle in degrees:"
            V "ang" = I
            I V "ang" E<= 0 {
                P "Error: Angle must be between 0 and 90."
            } J V "ang" E>= 90 {
                P "Error: Angle must be between 0 and 90."
            } H {
                P "Which side do you know? 1. Opposite 2. Adjacent 3. Hypotenuse"
                V "side_pick" = I
                I V "side_pick" E 1 {
                    P "Enter Opposite length:"
                    V "opp" = I
                    V "adj" = Mf(V "opp" / Mt(V "ang"))
                    V "hyp" = Mf(V "opp" / Ms(V "ang"))
                    P "Opposite: " + Mr(V "opp")
                    P "Adjacent: " + Mr(V "adj")
                    P "Hypotenuse: " + Mr(V "hyp")
                } J V "side_pick" E 2 {
                    P "Enter Adjacent length:"
                    V "adj" = I
                    V "opp" = Mf(V "adj" * Mt(V "ang"))
                    V "hyp" = Mf(V "adj" / Mc(V "ang"))
                    P "Opposite: " + Mr(V "opp")
                    P "Adjacent: " + Mr(V "adj")
                    P "Hypotenuse: " + Mr(V "hyp")
                } J V "side_pick" E 3 {
                    P "Enter Hypotenuse length:"
                    V "hyp" = I
                    V "opp" = Mf(V "hyp" * Ms(V "ang"))
                    V "adj" = Mf(V "hyp" * Mc(V "ang"))
                    P "Opposite: " + Mr(V "opp")
                    P "Adjacent: " + Mr(V "adj")
                    P "Hypotenuse: " + Mr(V "hyp")
                } H {
                    P "Invalid choice! Pick 1, 2, or 3."
                }
            }
        } K {
            P "Input error: " + V0
        }
    } J V "choice" E 2 {
        T {
            P "Enter Opposite side length:"
            V "opp" = I
            P "Enter Hypotenuse length:"
            V "hyp" = I
            I V "opp" E<= 0 {
                P "Error: Sides must be greater than 0."
            } J V "hyp" E<= V "opp" {
                P "Error: Hypotenuse must be longer than Opposite."
            } H {
                V "ratio" = Mf(V "opp" / V "hyp")
                V "deg" = Ma(V "ratio")
                P "sin(x): " + Mr(V "ratio")
                P "Angle x (deg): " + Mr(V "deg")
            }
        } K {
            P "Input error: " + V0
        }
    } H {
        P "Invalid mode! Type 1, 2, or e."
    }
}
```

### Condensed Version (Single-Line via `condense`)
```text
Mh=degreesV"running"=1CwV"running"E1{P"=== Welcome To Triggy 1.0 ==="P"1. normal_trig (finding sides)"P"2. reverse_trig (finding angles & ratios)"P"e. Exit"P"Type 1, 2, or e to pick:"V"choice"=IIV"choice"E"e"{P"Exiting Triggy 1.0. Goodbye!"V"running"=0}JV"choice"E1{T{P"Enter the known angle in degrees:"V"ang"=IIV"ang"E<=0{P"Error: Angle must be between 0 and 90."}JV"ang"E>=90{P"Error: Angle must be between 0 and 90."}H{P"Which side do you know? 1. Opposite 2. Adjacent 3. Hypotenuse"V"side_pick"=IIV"side_pick"E1{P"Enter Opposite length:"V"opp"=IV"adj"=Mf(V"opp"/Mt(V"ang"))V"hyp"=Mf(V"opp"/Ms(V"ang"))P"Opposite: "+Mr(V"opp")P"Adjacent: "+Mr(V"adj")P"Hypotenuse: "+Mr(V"hyp")}JV"side_pick"E2{P"Enter Adjacent length:"V"adj"=IV"opp"=Mf(V"adj"*Mt(V"ang"))V"hyp"=Mf(V"adj"/Mc(V"ang"))P"Opposite: "+Mr(V"opp")P"Adjacent: "+Mr(V"adj")P"Hypotenuse: "+Mr(V"hyp")}JV"side_pick"E3{P"Enter Hypotenuse length:"V"hyp"=IV"opp"=Mf(V"hyp"*Ms(V"ang"))V"adj"=Mf(V"hyp"*Mc(V"ang"))P"Opposite: "+Mr(V"opp")P"Adjacent: "+Mr(V"adj")P"Hypotenuse: "+Mr(V"hyp")}H{P"Invalid choice! Pick 1, 2, or 3."}}}K{P"Input error: "+V0}}JV"choice"E2{T{P"Enter Opposite side length:"V"opp"=IP"Enter Hypotenuse length:"V"hyp"=IIV"opp"E<=0{P"Error: Sides must be greater than 0."}JV"hyp"E<=V"opp"{P"Error: Hypotenuse must be longer than Opposite."}H{V"ratio"=Mf(V"opp"/V"hyp")V"deg"=Ma(V"ratio")P"sin(x): "+Mr(V"ratio")P"Angle x (deg): "+Mr(V"deg")}}K{P"Input error: "+V0}}H{P"Invalid mode! Type 1, 2, or e."}}
```