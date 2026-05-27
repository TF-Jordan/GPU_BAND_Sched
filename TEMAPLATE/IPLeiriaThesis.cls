% Simplified IPLeiriaThesis class — all configurations inlined.
% Based on: José Areia, IPLeiria Thesis v2.2.13
% Simplified by: Claude Code (structure only, visual output identical)

%%% Class Information %%%
\NeedsTeXFormat{LaTeX2e}
\ProvidesClass{IPLeiriaThesis}[2024/01/01 IPLeiria Thesis Class (Simplified)]

\RequirePackage{expl3}

%%% Handle Class Options %%%
\ExplSyntaxOn

\cs_new:Npn \IPLeiriaThesis_check_color:n #1 {
  \tl_set:Nn \l_tmpa_tl { #1 }
  \color_set:nn { \l_tmpa_tl } { #1 }
}

\keys_define:nn { IPLeiriaThesis } {
    school .choices:nn =
        { estg, esecs, esslei, esad, estm, utnfrba }
        { \tl_gset:Nn \g_IPLeiriaThesis_myschool_tl { #1 } },
    school / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}
        {Specified~'school'~not~available.~Defaulting~to~'estg'} },
    school .default:n = { estg },
    school .initial:n = { estg },

    coverstyle .choices:nn =
        { classic, bw }
        { \tl_gset:Nn \g_IPLeiriaThesis_mycover_tl { #1 } },
    coverstyle / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'coverstyle'~not~available.~Defaulting~to~'classic'} },
    coverstyle .default:n = { classic },
    coverstyle .initial:n = { classic },

    language .choices:nn =
        { english, portuguese, spanish, french }
        { \tl_gset:Nn \g_IPLeiriaThesis_mylanguage_tl { #1 } },
    language / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'language'~not~available.~Defaulting~to~'english'} },
    language .default:n = { english },
    language .initial:n = { english },

    chapterstyle .choices:nn =
        { classic, fancy, modern }
        { \tl_gset:Nn \g_IPLeiriaThesis_style_tl { #1 } },
    chapterstyle / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'chapterstyle'~not~available.~Defaulting~to~'classic'} },
    chapterstyle .default:n = { classic },
    chapterstyle .initial:n = { classic },

    docstage .choices:nn =
        { working, final }
        { \tl_gset:Nn \g_IPLeiriaThesis_docstage_tl { #1 } },
    docstage / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'docstage'~not~available.~Defaulting~to~'working'} },
    docstage .default:n = { working },
    docstage .initial:n = { working },

    media .choices:nn =
        { paper, screen }
        { \tl_gset:Nn \g_IPLeiriaThesis_media_tl { #1 } },
    media / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'media'~not~available.~Defaulting~to~'paper'} },
    media .default:n = { paper },
    media .initial:n = { paper },

    linkcolor .code:n = {
        \tl_gset:Nn \g_IPLeiriaThesis_linkcolor_tl { #1 }
        \IPLeiriaThesis_check_color:n { #1 }
        \tl_if_eq:NNTF \l_tmpa_tl \q_no_value {
            \tl_gset:Nn \g_IPLeiriaThesis_linkcolor_tl { black }
            \ClassWarningNoLine{IPLeiriaThesis}{Specified~'linkcolor'~not~available.~Defaulting~to~'black'}
        }
    },
    linkcolor .default:n = { black },
    linkcolor .initial:n = { black },

    doctype .choices:nn =
        { thesis, report }
        { \tl_gset:Nn \g_IPLeiriaThesis_doctype_tl { #1 } },
    doctype / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'doctype'~not~available.~Defaulting~to~'thesis'} },
    doctype .default:n = { thesis },
    doctype .initial:n = { thesis },

    bookprint .choices:nn =
        { true, false }
        { \tl_gset:Nn \g_IPLeiriaThesis_bookprint_tl { #1 } },
    bookprint / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'bookprint'~not~available.~Defaulting~to~'false'} },
    bookprint .default:n = { false },
    bookprint .initial:n = { false },

    aiacknowledgement .choices:nn =
        { true, false }
        { \tl_gset:Nn \g_IPLeiriaThesis_aiacknowledgement_tl { #1 } },
    aiacknowledgement / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'aiacknowledgement'~not~available.~Defaulting~to~'true'} },
    aiacknowledgement .default:n = { true },
    aiacknowledgement .initial:n = { true },

    listprefix .choices:nn =
        { true, false }
        { \tl_gset:Nn \g_IPLeiriaThesis_listprefix_tl { #1 } },
    listprefix / unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{Specified~'listprefix'~not~available.~Defaulting~to~'false'} },
    listprefix .default:n = { false },
    listprefix .initial:n = { false },

    unknown .code:n =
        { \ClassWarningNoLine{IPLeiriaThesis}{An~unknown~option~was~ignored.} }
}

\newcommand{\LanguageOption}{\g_IPLeiriaThesis_mylanguage_tl}
\newcommand{\SchoolOption}{\g_IPLeiriaThesis_myschool_tl}
\newcommand{\CoverOption}{\g_IPLeiriaThesis_mycover_tl}
\newcommand{\ChapterOption}{\g_IPLeiriaThesis_style_tl}
\newcommand{\DocStageOption}{\g_IPLeiriaThesis_docstage_tl}
\newcommand{\MediaOption}{\g_IPLeiriaThesis_media_tl}
\newcommand{\ColorOption}{\g_IPLeiriaThesis_linkcolor_tl}
\newcommand{\DocTypeOption}{\g_IPLeiriaThesis_doctype_tl}
\newcommand{\BookPrintOption}{\g_IPLeiriaThesis_bookprint_tl}
\newcommand{\AiAckOption}{\g_IPLeiriaThesis_aiacknowledgement_tl}
\newcommand{\ListPrefixOption}{\g_IPLeiriaThesis_listprefix_tl}

\ProcessKeyOptions[IPLeiriaThesis]
\ExplSyntaxOff

%%% Load Master Class %%%
\LoadClass[a4paper,11pt,twoside]{report}

%%% Load Babel Package (Language) %%%
\expandafter\RequirePackage\expandafter[\LanguageOption]{babel}

%%% Required Packages %%%
\RequirePackage{ifthen}
\RequirePackage{etoolbox}
\RequirePackage{graphicx}
\RequirePackage{subcaption}
\RequirePackage{tabularx}
\RequirePackage{xltabular}
\RequirePackage{booktabs}
\RequirePackage{multirow}
\RequirePackage{longtable}
\RequirePackage{caption}
\RequirePackage{enumitem}
\RequirePackage{amsmath}
\RequirePackage{amssymb}
\RequirePackage[dvipsnames]{xcolor}
\RequirePackage{tcolorbox}
\RequirePackage{varwidth}
\RequirePackage{titlesec}
\RequirePackage{titling}
\RequirePackage[toc,page]{appendix}
\RequirePackage[bookmarks,pdfusetitle]{hyperref}
\RequirePackage[symbols,acronym]{glossaries}
\RequirePackage[calc,datesep={/}]{datetime2}
\RequirePackage{xparse}
\RequirePackage{typearea}
\RequirePackage{eso-pic}
\RequirePackage{setspace}
\RequirePackage[newfloat]{minted}
\RequirePackage{silence}
\RequirePackage{fontawesome5}
\RequirePackage{calc}
\RequirePackage{soul}
\RequirePackage{mfirstuc}
\RequirePackage{tikz}
\RequirePackage{psvectorian}
\RequirePackage{blindtext}

\tcbuselibrary{most}
\WarningsOff[blindtext]

% =============================================================================
% Config 00: Fonts
% =============================================================================
\usepackage[T1]{fontenc}
\usepackage{newpxmath}
\usepackage{fvextra}
\usepackage{csquotes}
\usepackage{fontspec}

\linespread{1.2}
\setmainfont{TeX Gyre Pagella}[Ligatures=TeX,LetterSpace=0.7]
\newcommand{\garamondfont}{\fontspec{EB Garamond}}
\newcommand{\latofont}{\fontspec{Lato}}
\newcommand{\crimsonfont}{\fontspec{TeX Gyre Pagella}[LetterSpace=-0.5]}

% =============================================================================
% Config 01: Colors
% =============================================================================
\colorlet{maincolor}{\ColorOption}
\colorlet{chapterdarkred}{red!30!black}
\colorlet{chaptergray}{gray!80!white}
\definecolor{frontpagedark}{HTML}{1c1c1c}
\definecolor{note}{HTML}{0969da}
\definecolor{tip}{HTML}{1a7f37}
\definecolor{warning}{HTML}{9a6700}
\definecolor{todo}{HTML}{d1242f}
\definecolor{important}{HTML}{8250df}

% =============================================================================
% Config 02: Margins
% =============================================================================
\usepackage{geometry}
\ifthenelse{\equal{\BookPrintOption}{false}}{%
    \geometry{
        paper=a4paper,
        top=2.54cm,
        bottom=2.54cm,
        left=3.15cm,
        right=3.15cm,
        footskip=1.6cm,
        headsep=0.75cm,
    }
}{%
    \geometry{
        paper=a4paper,
        top=2.54cm,
        bottom=2.54cm,
        inner=2.54cm,
        outer=3.81cm,
        footskip=1.6cm,
        headsep=0.75cm,
        bindingoffset=0.5cm,
    }
}
\addtolength{\textheight}{-20pt}
\setlength{\headheight}{20pt}

% =============================================================================
% Config 03: References (biblatex)
% =============================================================================
\usepackage[
    backend=biber,
    citestyle=numeric,
    bibstyle=numeric,
    sorting=none,
    natbib=true,
    uniquelist=false,
    urldate=iso,
    seconds=true
]{biblatex}

\setlength\bibitemsep{1.5\itemsep}
\setlength\bibhang{0pt}
\addbibresource{bibliography/references.bib}

\ExecuteBibliographyOptions{maxcitenames=1}
\DeclareFieldFormat{citehyperref}{%
    \DeclareFieldAlias{bibhyperref}{noformat}%
    \bibhyperref{#1}%
}
\DeclareFieldFormat{textcitehyperref}{%
    \DeclareFieldAlias{bibhyperref}{noformat}%
    \bibhyperref{%
        #1%
        \ifbool{cbx:parens}
        {\bibcloseparen\global\boolfalse{cbx:parens}}
    {}}%
}
\savebibmacro{cite}
\savebibmacro{textcite}
\renewbibmacro*{cite}{%
    \printtext[citehyperref]{%
        \restorebibmacro{cite}%
        \usebibmacro{cite}%
    }}
\renewbibmacro*{textcite}{%
    \ifboolexpr{
        ( not test {\iffieldundef{prenote}} and
          test {\ifnumequal{\value{citecount}}{1}} )
        or
        ( not test {\iffieldundef{postnote}} and
          test {\ifnumequal{\value{citecount}}{\value{citetotal}}} )
    }
    {\DeclareFieldAlias{textcitehyperref}{noformat}}
    {}%
    \printtext[textcitehyperref]{%
    \restorebibmacro{textcite}%
    \usebibmacro{textcite}}%
}

\hypersetup{
    bookmarksnumbered,
    colorlinks=true,
    urlcolor=maincolor,
    linkcolor=maincolor,
    citecolor=maincolor,
}
\def\Hy@numberline#1{\ifHy@bookmarksnumbered{#1.\hspace{0.3em}}\else#1\fi}

% =============================================================================
% Config 04: Headers
% =============================================================================
\usepackage{fancyhdr}
\usepackage[bottom,hang]{footmisc}
\pagestyle{fancy} \fancyhf{}
\renewcommand{\headrulewidth}{.3pt}
\setlength{\footnotemargin}{6pt}

\renewcommand{\chaptermark}[1]{\markboth{\textit{\thechapter. #1}}{}}
\renewcommand{\sectionmark}[1]{\markright{\textit{\thesection. #1}}}

\fancyhead[OLH]{\ifnum\value{section}=0\leftmark\else\rightmark\fi\vspace{3pt}}
\fancyhead[ERH]{\leftmark\vspace{3pt}}
\fancyhead[ORH,ELH]{\thepage\vspace{3pt}}

\fancypagestyle{plain}{%
    \fancyhf{}
    \renewcommand{\headrulewidth}{0pt}
    \fancyfoot[ORF,ELF]{\thepage\vspace{3pt}}
    \ifthenelse{\equal{\DocStageOption}{working}}{
        \fancyfoot[OCF,ECF]{\small\GetDocumentVersion~-~\textit{\today}}
    }{}
}
\ifthenelse{\equal{\DocStageOption}{working}}{%
    \fancyfoot[OCF,ECF]{\small\GetDocumentVersion~-~\textit{\today}}%
}{}

% =============================================================================
% Config 05: Table of Contents
% =============================================================================
\newcommand{\bookmarktocentry}{\pdfbookmark[0]{\contentsname}{toc}}
\ProvideDocumentCommand\MakeLinkTarget{sO{}m}{}
\patchcmd{\listoffigures}
{\chapter*{\listfigurename}}
{\chapter*{\listfigurename}\MakeLinkTarget*{listoffigures}%
\addcontentsline{toc}{frontmatter}{\listfigurename}}{}{}

\patchcmd{\listoftables}
{\chapter*{\listtablename}}
{\chapter*{\listtablename}\MakeLinkTarget*{listoftables}%
\addcontentsline{toc}{frontmatter}{\listtablename}}{}{}

\NewCommandCopy\l@frontmatter\l@chapter
\NewCommandCopy\l@backmatter\l@chapter

\providecommand*{\toclevel@frontmatter}{0}
\providecommand*{\toclevel@backmatter}{0}

\patchcmd{\l@frontmatter}{\bfseries}{\itshape}{}{}
\patchcmd{\l@frontmatter}{\nobreak\hfil}{\upshape\nobreak\hfil}{}{}
\patchcmd{\l@frontmatter}{1.0em}{3pt}{}{}
\patchcmd{\l@backmatter}{\bfseries}{\itshape}{}{}
\patchcmd{\l@backmatter}{\nobreak\hfil}{\upshape\nobreak\hfil}{}{}

\defbibheading{bibintoc}{\chapter*{#1}\addcontentsline{toc}{backmatter}{\refname}}

\newcommand{\glossarytoc}{\MakeLinkTarget*{glossary}%
\addcontentsline{toc}{frontmatter}{\glossaryname}}

\newcommand{\acronymtoc}{\MakeLinkTarget*{acronym}%
\addcontentsline{toc}{frontmatter}{\acronymname}}

\newcommand{\symboltoc}{\MakeLinkTarget*{symbol}%
\addcontentsline{toc}{frontmatter}{\glssymbolsgroupname}}

\ifthenelse{\equal{\ListPrefixOption}{true}}{%
    \renewcommand*\l@figure[2]{\@dottedtocline{1}{0em}{2.3em}{%
        \textcolor{red!45!black}{\figurename\ \,#1}}{#2}%
    }
    \renewcommand*\l@table[2]{\@dottedtocline{1}{0em}{2.3em}{%
        \textcolor{red!45!black}{\tablename\ \,#1}}{#2}%
    }
}{
    \renewcommand*\l@figure{\@dottedtocline{1}{0em}{2.3em}}
    \let\l@table\l@figure
}

% =============================================================================
% Config 06: Glossary
% =============================================================================
\setglossarystyle{long}
\setlength\LTleft{-6pt}
\setlength\LTright{0pt}
\setlength\glsdescwidth{0.94\hsize}
\renewcommand{\glsnamefont}[1]{\textbf{#1}}
\renewcommand{\glossaryentrynumbers}[1]%
{\textit{(p. \textcolor{maincolor}{#1})}}
\newcommand{\symboltype}{symbols}

% =============================================================================
% Config 07: Chapters and Sections
% =============================================================================
\ifthenelse{\equal{\ChapterOption}{classic}}{
    \titleformat{\chapter}[display]%
    {\raggedleft\huge}%
    {\raggedleft\fontsize{98}{10}\bfseries{\color{chapterdarkred}{\thechapter}}}%
    {0pt}%
    {\hspace{3cm}\crimsonfont\fontsize{26.8}{30}\selectfont}%
}{}

\ifthenelse{\equal{\ChapterOption}{fancy}}{
    \titleformat{\chapter}[display]
    {\flushright\normalsize\huge\color{black}}%
    {\flushright\normalsize%
    {\fontsize{25}{25}\selectfont\hspace*{-1em}\rule[-2pt]{.1pt}{30pt}\hspace{0.5em}\raisebox{4.5pt}{\thechapter}}}%
    {10pt}%
    {\addfontfeature{LetterSpace=6}\huge\scshape}%
}{}

\ifthenelse{\equal{\ChapterOption}{modern}}{
    \titleformat{\chapter}[display]
    {\normalsize\huge\color{black}}%
    {\crimsonfont\flushright\fontsize{84}{10}\bfseries%
    {\color{chaptergray}\raisebox{5pt}{\thechapter}}%
    }{10pt}%
    {\crimsonfont\addfontfeature{LetterSpace=6}\fontsize{28}{32}\MakeUppercase}%
}{}

% =============================================================================
% Config 08: Tables
% =============================================================================
\newcommand{\customtableformatting}{\renewcommand{\arraystretch}{1.1}\small}
\AtBeginEnvironment{tabular}{\customtableformatting}
\AtBeginEnvironment{tabularx}{\customtableformatting}
\preto\longtable{\customtableformatting}

\renewcommand\tabularxcolumn[1]{m{#1}}
\newcolumntype{C}{>{\centering\arraybackslash}X}
\newcolumntype{L}{>{\raggedright\arraybackslash}X}
\newcolumntype{R}{>{\raggedleft\arraybackslash}X}

\captionsetup{skip=6pt}
\captionsetup{
    labelfont={bf,small},
    textfont={it,small}
}
\captionsetup[table]{skip=4pt}

\setlist{noitemsep}

% =============================================================================
% Config 09: Code Highlighting
% =============================================================================
\newenvironment{code}{\captionsetup{type=listing}}{}
\ifthenelse{\equal{\LanguageOption}{portuguese}}{%
    \SetupFloatingEnvironment{listing}{name=Listagem,within=none}
}{\ifthenelse{\equal{\LanguageOption}{spanish}}{%
    \SetupFloatingEnvironment{listing}{name=Listado,within=none}
}{%
    \SetupFloatingEnvironment{listing}{name=Listing,within=none}
}}
\captionsetup[listing]{position=above,skip=-6pt}
\newenvironment{longlisting}{\bigskip\medskip\captionsetup{type=listing}}{}

\setminted{
    frame=lines,
    framesep=2mm,
    baselinestretch=1.2,
    fontsize=\footnotesize,
    linenos,
    tabsize=4,
    breaklines=true,
    breakanywhere=true
}

\renewcommand{\theFancyVerbLine}{%
    \scriptsize\textcolor[rgb]{0.7,0.7,0.7}{\arabic{FancyVerbLine}}%
}

% =============================================================================
% Config 10: Macros
% =============================================================================

\newcommand{\blankpage}{\clearpage\null\thispagestyle{empty}\newpage}
\newcommand{\plainblankpage}{\thispagestyle{plain}\blankpage}

% Writing guidance
\newcommand{\guideinfo}[1]{%
    \ifthenelse{\equal{\LanguageOption}{portuguese}}{%
        \noindent\textcolor{note}{\faInfoCircle\ \textbf{Orientação Para a Escrita}}%
    }{\ifthenelse{\equal{\LanguageOption}{spanish}}{%
        \noindent\textcolor{note}{\faInfoCircle\ \textbf{Guía de Escritura}}%
    }{
        \noindent\textcolor{note}{\faInfoCircle\ \textbf{Writing Guidance}}%
    }}
    \\[.55em]%
    \noindent #1%
}

\newcommand{\exampleinfo}[1]{%
    \ifthenelse{\equal{\LanguageOption}{portuguese}}{%
        \noindent\textcolor{black}{\faInfoCircle\ \textbf{Exemplo de Utilização}}%
    }{\ifthenelse{\equal{\LanguageOption}{spanish}}{%
        \noindent\textcolor{black}{\faInfoCircle\ \textbf{Ejemplo de Uso}}%
    }{
        \noindent\textcolor{black}{\faInfoCircle\ \textbf{Example of Use}}%
    }}
    \\[.55em]%
    \noindent #1%
}

% Abstract keywords
\NewDocumentCommand{\keywordsen}{m}{\vspace{.4cm}\noindent\textbf{Keywords:} #1}
\NewDocumentCommand{\keywordspt}{m}{\vspace{.4cm}\noindent\textbf{Mots-Clés:} #1}
\NewDocumentCommand{\keywordses}{m}{\vspace{.4cm}\noindent\textbf{Mots-Clés:} #1}

% Landscape mode
\def\ifGm@preamble#1{\@firstofone}
\appto\restoregeometry{%
    \pdfpagewidth=\paperwidth
    \pdfpageheight=\paperheight}
\apptocmd\newgeometry{%
    \pdfpagewidth=\paperwidth
    \pdfpageheight=\paperheight}{}{}
\newenvironment{landscapemode}[2]{%
    \newgeometry{paperwidth=#1,paperheight=#2,hmargin=3cm,vmargin=5cm,top=3cm,landscape}
    \fancyheadoffset{0pt}
}{%
    \restoregeometry
    \fancyheadoffset{0pt}
}

% Custom block (note, tip, warning, todo, important)
\newtcolorbox[auto counter,number within=section]{block}[1][]{
    enhanced jigsaw,
    coltitle=#1,
    colback=#1!08!white,
    colframe=#1!08!white,
    fonttitle=\latofont\bfseries,
    sharp corners=west,
    detach title,
    borderline west={.85mm}{0pt}{#1},
    pad at break=1mm,
    top=4mm,
    bottom=4mm,
    title={\ifstrequal{#1}{todo}{To-Do}{\capitalisewords{#1}}},
    code={\ifdefempty{\tcbtitletext}{}{\tcbset{before upper={\tcbtitle\par\medskip}}}},
}

% Autoref capitalisation
\addto\extrasenglish{\def\figureautorefname{Figure}}
\addto\extrasenglish{\def\chapterautorefname{Chapter}}
\addto\extrasenglish{\def\sectionautorefname{Section}}
\addto\extrasenglish{\def\subsectionautorefname{Section}}
\addto\extrasenglish{\def\subsubsectionautorefname{Section}}
\addto\extrasenglish{\def\tableautorefname{Table}}
\addto\extrasenglish{\def\partautorefname{Part}}
\addto\extrasenglish{\def\appendixautorefname{Appendix}}
\addto\extrasportuguese{\def\sectionautorefname{Secção}}
\addto\extrasportuguese{\def\subsectionautorefname{Secção}}
\addto\extrasportuguese{\def\subsubsectionautorefname{Secção}}
\addto\extrasspanish{\def\figureautorefname{Figura}}
\addto\extrasspanish{\def\chapterautorefname{Capítulo}}
\addto\extrasspanish{\def\sectionautorefname{Sección}}
\addto\extrasspanish{\def\subsectionautorefname{Sección}}
\addto\extrasspanish{\def\subsubsectionautorefname{Sección}}
\addto\extrasspanish{\def\tableautorefname{Tabla}}
\addto\extrasspanish{\def\partautorefname{Parte}}
\addto\extrasspanish{\def\appendixautorefname{Apéndice}}

% Color overwrite
\newcommand{\overwritecolor}[2]{%
    \textcolor{#1}{%
        \renewcommand\color[2][]{}%
    #2}%
}

% Glossary/acronym wrappers
\newcommand{\printnormalglossary}{\printglossary}
\newcommand{\printacronymglossary}{\printglossary[type=\acronymtype]}
\newcommand{\printsymbolglossary}{\printglossary[type=symbols]}

% Media-dependent blank pages
\newcommand{\MediaOptionLogic}{%
    \ifthenelse{\equal{\MediaOption}{paper}}{\plainblankpage}{\thispagestyle{plain}}%
}
\newcommand{\MediaOptionLogicAcr}{%
    \ifthenelse{\equal{\MediaOption}{paper}}{\plainblankpage}{\clearpage\thispagestyle{plain}}%
}

\newcommand{\MediaOptionLogicAnnexes}{%
    \ifthenelse{\equal{\MediaOption}{paper}}{\blankpage}{\clearpage}%
}

% Removed apptocmd for TOC/LOF/LOT to avoid infinite loop with French babel
\apptocmd{\printnormalglossary}{\MediaOptionLogic}{}{\fail}
\apptocmd{\printacronymglossary}{\MediaOptionLogicAcr}{}{\fail}
\apptocmd{\printsymbolglossary}{\MediaOptionLogicAcr}{}{\fail}
\pretocmd{\printbibliography}{\MediaOptionLogic}{}{\fail}

\NewDocumentCommand{\prompt}{o m}{%
    \ifthenelse{\equal{\LanguageOption}{portuguese}}{%
        \IfNoValueTF{#1}
        {\noindent\textbf{Instrução:} #2}
        {\noindent\textbf{Instrução #1:} #2}
    }{\ifthenelse{\equal{\LanguageOption}{spanish}}{%
        \IfNoValueTF{#1}
        {\noindent\textbf{Instrucción:} #2}
        {\noindent\textbf{Instrucción #1:} #2}
    }{
        \IfNoValueTF{#1}
        {\noindent\textbf{Prompt:} #2}
        {\noindent\textbf{Prompt #1:} #2}
    }}
}

\NewDocumentCommand{\aioutput}{o m}{%
    \ifthenelse{\equal{\LanguageOption}{portuguese}}{%
        \IfNoValueTF{#1}
        {\noindent\textbf{Resposta:} #2}
        {\noindent\textbf{Resposta #1:} #2}
    }{\ifthenelse{\equal{\LanguageOption}{spanish}}{%
        \IfNoValueTF{#1}
        {\noindent\textbf{Respuesta:} #2}
        {\noindent\textbf{Respuesta #1:} #2}
    }{
        \IfNoValueTF{#1}
        {\noindent\textbf{Output:} #2}
        {\noindent\textbf{Output #1:} #2}
    }}
}

% =============================================================================
% Config 11: Metadata commands
% =============================================================================
\NewDocumentCommand{\FirstAuthor}{m}{\newcommand{\GetFirstAuthor}{#1}}
\NewDocumentCommand{\FirstAuthorNumber}{m}{\newcommand{\GetFirstAuthorNumber}{#1}}
\NewDocumentCommand{\SecondAuthor}{m}{\newcommand{\GetSecondAuthor}{#1}}
\NewDocumentCommand{\SecondAuthorNumber}{m}{\newcommand{\GetSecondAuthorNumber}{#1}}
\NewDocumentCommand{\ThirdAuthor}{m}{\newcommand{\GetThirdAuthor}{#1}}
\NewDocumentCommand{\ThirdAuthorNumber}{m}{\newcommand{\GetThirdAuthorNumber}{#1}}
\NewDocumentCommand{\Supervisor}{m}{\newcommand{\GetSupervisor}{#1}}
\NewDocumentCommand{\SupervisorMail}{m}{\newcommand{\GetSupervisorMail}{#1}}
\NewDocumentCommand{\SupervisorTitle}{m}{\newcommand{\GetSupervisorTitle}{#1}}
\NewDocumentCommand{\CoSupervisor}{m}{\newcommand{\GetCoSupervisor}{#1}}
\NewDocumentCommand{\CoSupervisorMail}{m}{\newcommand{\GetCoSupervisorMail}{#1}}
\NewDocumentCommand{\CoSupervisorTitle}{m}{\newcommand{\GetCoSupervisorTitle}{#1}}
\NewDocumentCommand{\SecCoSupervisor}{m}{\newcommand{\GetSecCoSupervisor}{#1}}
\NewDocumentCommand{\SecCoSupervisorMail}{m}{\newcommand{\GetSecCoSupervisorMail}{#1}}
\NewDocumentCommand{\SecCoSupervisorTitle}{m}{\newcommand{\GetSecCoSupervisorTitle}{#1}}
\NewDocumentCommand{\Subtitle}{m}{\newcommand{\GetSubtitle}{#1}}
\NewDocumentCommand{\University}{m}{\newcommand{\GetUniversity}{#1}}
\NewDocumentCommand{\School}{m}{\newcommand{\GetSchool}{#1}}
\NewDocumentCommand{\Department}{m}{\newcommand{\GetDepartment}{#1}}
\NewDocumentCommand{\Degree}{m}{\newcommand{\GetDegree}{#1}}
\NewDocumentCommand{\Course}{m}{\newcommand{\GetCourse}{#1}}
\NewDocumentCommand{\ThesisType}{m}{\newcommand{\GetThesisType}{#1}}
\NewDocumentCommand{\AcademicYear}{m}{\newcommand{\GetAcademicYear}{#1}}

\let\Title\title
\newcommand{\GetTitle}{\@title}
\let\Date\date
\newcommand{\GetDate}{\@date}

\NewDocumentCommand{\DocumentVersion}{m}{%
    \newcommand{\GetDocumentVersion}{\color{black}{%
        \ifthenelse{\equal{\LanguageOption}{portuguese}}{%
            \textit{Versão Preliminar #1}%
        }{\ifthenelse{\equal{\LanguageOption}{spanish}}{%
            \textit{Versión Preliminar #1}%
        }{%
            \textit{Draft Version #1}%
        }}%
    }}%
}

\AfterPreamble{
    \hypersetup{
        pdfauthor={\GetFirstAuthor},
        pdftitle={\GetTitle},
        pdfproducer={LaTeX},
        pdfcreator={XeLaTeX}
    }
}
