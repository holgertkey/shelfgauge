//! Readable names of FB2 genre codes.
//!
//! The codes are those of `FictionBookGenres.xsd` (the FictionBook 2 schema,
//! <https://github.com/gribuser/fb2>) plus a few widespread later ones (`sf_etc`,
//! `sf_postapocalyptic`, …). The schema defines no names; these are written for this
//! table. Unknown codes, and EPUB subjects (free text), are shown as they are.

/// `(code, name)`, sorted by code.
const GENRES: &[(&str, &str)] = &[
    ("accounting", "Accounting"),
    ("adv_animal", "Nature and animals"),
    ("adv_geo", "Travel and geography"),
    ("adv_history", "Historical adventure"),
    ("adv_indian", "Native American adventure"),
    ("adv_maritime", "Sea adventure"),
    ("adv_western", "Western"),
    ("adventure", "Adventure"),
    ("antique", "Old literature"),
    ("antique_ant", "Ancient literature"),
    ("antique_east", "Old Eastern literature"),
    ("antique_european", "Old European literature"),
    ("antique_myths", "Myths and legends"),
    ("antique_russian", "Old Russian literature"),
    ("aphorism_quote", "Aphorisms and quotations"),
    ("architecture_book", "Architecture"),
    ("auto_regulations", "Cars and traffic rules"),
    ("banking", "Banking"),
    ("beginning_authors", "Beginning authors"),
    ("child_adv", "Adventure for children"),
    ("child_det", "Detective stories for children"),
    ("child_education", "Education for children"),
    ("child_prose", "Prose for children"),
    ("child_sf", "Science fiction for children"),
    ("child_tale", "Fairy tales"),
    ("child_verse", "Poetry for children"),
    ("children", "Children's books"),
    ("cinema_theatre", "Cinema and theatre"),
    ("city_fantasy", "Urban fantasy"),
    ("comp_db", "Databases"),
    ("comp_hard", "Computer hardware"),
    ("comp_osnet", "Operating systems and networks"),
    ("comp_programming", "Programming"),
    ("comp_soft", "Software"),
    ("comp_www", "Internet"),
    ("computers", "Computers"),
    ("design", "Art and design"),
    ("det_action", "Action thriller"),
    ("det_classic", "Classic detective"),
    ("det_crime", "Crime"),
    ("det_espionage", "Espionage"),
    ("det_hard", "Hard-boiled detective"),
    ("det_history", "Historical detective"),
    ("det_irony", "Ironic detective"),
    ("det_maniac", "Serial killer detective"),
    ("det_police", "Police procedural"),
    ("det_political", "Political detective"),
    ("detective", "Detective"),
    ("dragon_fantasy", "Dragon fantasy"),
    ("dramaturgy", "Drama"),
    ("economics", "Economics"),
    ("essays", "Essays"),
    ("fantasy_fight", "Battle fantasy"),
    ("foreign_action", "Foreign action"),
    ("foreign_adventure", "Foreign adventure"),
    ("foreign_antique", "Foreign old literature"),
    ("foreign_business", "Foreign business literature"),
    ("foreign_children", "Foreign children's books"),
    ("foreign_comp", "Foreign computer literature"),
    ("foreign_contemporary", "Foreign contemporary fiction"),
    (
        "foreign_contemporary_lit",
        "Foreign contemporary literature",
    ),
    ("foreign_desc", "Foreign reference literature"),
    ("foreign_detective", "Foreign detective"),
    ("foreign_dramaturgy", "Foreign drama"),
    ("foreign_edu", "Foreign educational literature"),
    ("foreign_fantasy", "Foreign fantasy"),
    ("foreign_home", "Foreign home and family"),
    ("foreign_humor", "Foreign humor"),
    ("foreign_language", "Foreign languages"),
    ("foreign_love", "Foreign romance"),
    ("foreign_novel", "Foreign novels"),
    ("foreign_other", "Other foreign literature"),
    ("foreign_poetry", "Foreign poetry"),
    ("foreign_prose", "Foreign prose"),
    ("foreign_psychology", "Foreign psychology"),
    ("foreign_publicism", "Foreign journalism"),
    ("foreign_religion", "Foreign religious literature"),
    ("foreign_sf", "Foreign science fiction"),
    ("geo_guides", "Travel guides"),
    ("geography_book", "Geography"),
    ("global_economy", "Global economy"),
    ("historical_fantasy", "Historical fantasy"),
    ("home", "Home and family"),
    ("home_cooking", "Cooking"),
    ("home_crafts", "Hobbies and crafts"),
    ("home_diy", "Do it yourself"),
    ("home_entertain", "Entertainment"),
    ("home_garden", "Garden"),
    ("home_health", "Health"),
    ("home_pets", "Pets"),
    ("home_sex", "Sex and family"),
    ("home_sport", "Sports"),
    ("humor", "Humor"),
    ("humor_anecdote", "Jokes"),
    ("humor_fantasy", "Humorous fantasy"),
    ("humor_prose", "Humorous prose"),
    ("humor_verse", "Humorous verse"),
    ("industries", "Industries"),
    ("job_hunting", "Job hunting"),
    ("literature_18", "18th-century literature"),
    ("literature_19", "19th-century literature"),
    ("literature_20", "20th-century literature"),
    ("love_contemporary", "Contemporary romance"),
    ("love_detective", "Romantic suspense"),
    ("love_erotica", "Erotica"),
    ("love_fantasy", "Romantic fantasy"),
    ("love_history", "Historical romance"),
    ("love_sf", "Science fiction romance"),
    ("love_short", "Short romance"),
    ("magician_book", "Magic and wizards"),
    ("management", "Management"),
    ("marketing", "Marketing"),
    ("military_special", "Military"),
    ("music_dancing", "Music and dance"),
    ("narrative", "Narrative"),
    ("newspapers", "Newspapers"),
    ("nonf_biography", "Biography and memoirs"),
    ("nonf_criticism", "Criticism"),
    ("nonf_publicism", "Journalism"),
    ("nonfiction", "Nonfiction"),
    ("org_behavior", "Organizational behavior"),
    ("paper_work", "Office work"),
    ("pedagogy_book", "Pedagogy"),
    ("periodic", "Periodicals"),
    ("personal_finance", "Personal finance"),
    ("poetry", "Poetry"),
    ("popadanec", "Time travelers"),
    ("popular_business", "Popular business"),
    ("prose_classic", "Classic prose"),
    ("prose_contemporary", "Contemporary prose"),
    ("prose_counter", "Counterculture"),
    ("prose_history", "Historical prose"),
    ("prose_military", "Military prose"),
    ("prose_rus_classic", "Russian classic prose"),
    ("prose_su_classics", "Soviet classic prose"),
    ("psy_alassic", "Classic psychology"),
    ("psy_childs", "Child psychology"),
    ("psy_generic", "Psychology"),
    ("psy_personal", "Personal growth"),
    ("psy_sex_and_family", "Sex and family psychology"),
    ("psy_social", "Social psychology"),
    ("psy_theraphy", "Psychotherapy"),
    ("real_estate", "Real estate"),
    ("ref_dict", "Dictionaries"),
    ("ref_encyc", "Encyclopedias"),
    ("ref_guide", "Guides"),
    ("ref_ref", "Reference"),
    ("reference", "Reference books"),
    ("religion", "Religion"),
    ("religion_esoterics", "Esoterics"),
    ("religion_rel", "Religion"),
    ("religion_self", "Self-improvement"),
    ("russian_contemporary", "Russian contemporary literature"),
    ("russian_fantasy", "Russian fantasy"),
    ("sci_biology", "Biology"),
    ("sci_chem", "Chemistry"),
    ("sci_culture", "Cultural studies"),
    ("sci_history", "History"),
    ("sci_juris", "Law"),
    ("sci_linguistic", "Linguistics"),
    ("sci_math", "Mathematics"),
    ("sci_medicine", "Medicine"),
    ("sci_philosophy", "Philosophy"),
    ("sci_phys", "Physics"),
    ("sci_politics", "Politics"),
    ("sci_religion", "Religious studies"),
    ("sci_tech", "Technology"),
    ("science", "Science"),
    ("sf", "Science fiction"),
    ("sf_action", "Action science fiction"),
    ("sf_cyberpunk", "Cyberpunk"),
    ("sf_detective", "Science fiction detective"),
    ("sf_epic", "Epic fantasy"),
    ("sf_etc", "Other science fiction"),
    ("sf_fantasy", "Fantasy"),
    ("sf_heroic", "Heroic fantasy"),
    ("sf_history", "Alternate history"),
    ("sf_horror", "Horror"),
    ("sf_humor", "Humorous science fiction"),
    ("sf_litrpg", "LitRPG"),
    ("sf_postapocalyptic", "Post-apocalyptic"),
    ("sf_social", "Social science fiction"),
    ("sf_space", "Space fiction"),
    ("sf_stimpank", "Steampunk"),
    ("short_story", "Short stories"),
    ("sketch", "Sketches"),
    ("small_business", "Small business"),
    ("sociology_book", "Sociology"),
    ("stock", "Stock market"),
    ("thriller", "Thriller"),
    ("upbringing_book", "Parenting"),
    ("vampire_book", "Vampires"),
    ("visual_arts", "Visual arts"),
];

/// The readable name of an FB2 genre code (compared case-insensitively, trimmed), or
/// `None` for a code not in the table.
///
/// # Examples
///
/// ```
/// use shelfgauge::format::fb2::genres::genre_name;
///
/// assert_eq!(genre_name("sf_fantasy"), Some("Fantasy"));
/// assert_eq!(genre_name(" SF_Fantasy "), Some("Fantasy"));
/// assert_eq!(genre_name("Fiction / Classics"), None);
/// ```
pub fn genre_name(code: &str) -> Option<&'static str> {
    let code = code.trim().to_ascii_lowercase();
    GENRES
        .binary_search_by(|(c, _)| (*c).cmp(code.as_str()))
        .ok()
        .map(|i| GENRES[i].1)
}

/// How a genre is shown: its name, or the code itself if it has none.
pub fn genre_label(code: &str) -> String {
    genre_name(code).map_or_else(|| code.to_owned(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_unique() {
        assert!(GENRES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(
            GENRES
                .iter()
                .all(|(c, n)| !c.is_empty() && c.trim() == *c && !n.is_empty())
        );
    }

    #[test]
    fn every_schema_code_has_a_name() {
        // FictionBookGenres.xsd, except `unrecognised` (shown as it is).
        let schema = "accounting adv_animal adv_geo adv_history adv_maritime adv_western \
            adventure antique antique_ant antique_east antique_european antique_myths \
            antique_russian aphorism_quote architecture_book auto_regulations banking \
            beginning_authors child_adv child_det child_education child_prose child_sf \
            child_tale child_verse children cinema_theatre city_fantasy comp_db comp_hard \
            comp_osnet comp_programming comp_soft comp_www computers design det_action \
            det_classic det_crime det_espionage det_hard det_history det_irony det_police \
            det_political detective dragon_fantasy dramaturgy economics essays fantasy_fight \
            foreign_action foreign_adventure foreign_antique foreign_business \
            foreign_children foreign_comp foreign_contemporary foreign_contemporary_lit \
            foreign_desc foreign_detective foreign_dramaturgy foreign_edu foreign_fantasy \
            foreign_home foreign_humor foreign_language foreign_love foreign_novel \
            foreign_other foreign_poetry foreign_prose foreign_psychology foreign_publicism \
            foreign_religion foreign_sf geo_guides geography_book global_economy \
            historical_fantasy home home_cooking home_crafts home_diy home_entertain \
            home_garden home_health home_pets home_sex home_sport humor humor_anecdote \
            humor_fantasy humor_prose humor_verse industries job_hunting literature_18 \
            literature_19 literature_20 love_contemporary love_detective love_erotica \
            love_fantasy love_history love_sf love_short magician_book management marketing \
            military_special music_dancing narrative newspapers nonf_biography \
            nonf_criticism nonf_publicism nonfiction org_behavior paper_work pedagogy_book \
            periodic personal_finance poetry popadanec popular_business prose_classic \
            prose_counter prose_history prose_military prose_rus_classic prose_su_classics \
            psy_alassic psy_childs psy_generic psy_personal psy_sex_and_family psy_social \
            psy_theraphy real_estate ref_dict ref_encyc ref_guide ref_ref reference religion \
            religion_esoterics religion_rel religion_self russian_contemporary \
            russian_fantasy sci_biology sci_chem sci_culture sci_history sci_juris \
            sci_linguistic sci_math sci_medicine sci_philosophy sci_phys sci_politics \
            sci_religion sci_tech science sf sf_action sf_cyberpunk sf_detective sf_fantasy \
            sf_heroic sf_history sf_horror sf_humor sf_social sf_space short_story sketch \
            small_business sociology_book stock thriller upbringing_book vampire_book \
            visual_arts";
        let codes: Vec<&str> = schema.split_whitespace().collect();
        assert_eq!(codes.len(), 185);
        for code in codes {
            assert!(genre_name(code).is_some(), "{code}");
        }
    }

    #[test]
    fn labels_fall_back_to_the_code() {
        assert_eq!(genre_label("det_police"), "Police procedural");
        assert_eq!(genre_label("Fiction / Classics"), "Fiction / Classics");
        assert_eq!(genre_label("unrecognised"), "unrecognised");
    }
}
